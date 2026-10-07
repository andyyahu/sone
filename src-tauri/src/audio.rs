use crate::audio_output::{AudioOutputConfig, AudioOutputRoute, AudioOutputState};
#[cfg(target_os = "linux")]
use crate::camilla_fir::{FirOutput, FirSlot};
use crate::signal_path::SignalPathTracker;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use tauri::Emitter;

/// Read the real GStreamer registry.
///
/// `gst::init()` is idempotent, and `main.rs` sets `GST_PLUGIN_PATH` before
/// `run()`, so calling this from `AppState::new` sees the same registry the
/// audio thread will. Note this adds a registry scan to the startup path,
/// where today `gst::init()` runs only on the audio thread.
pub fn probe_host_caps() -> crate::proxy::HostCaps {
    if let Err(e) = gst::init() {
        // A registry that cannot be read is not a reason to assume the best.
        // Reporting nothing present makes `route()` refuse the audio
        // capabilities, so the user is told rather than played unproxied.
        log::error!("[audio] GStreamer init failed; reporting no audio capability: {e}");
        return crate::proxy::HostCaps {
            has_dashdemux: false,
            has_curlhttpsrc: false,
            gst_version: (0, 0, 0),
        };
    }
    let (major, minor, micro, _nano) = gst::version();
    crate::proxy::HostCaps {
        has_dashdemux: gst::ElementFactory::find("dashdemux").is_some(),
        has_curlhttpsrc: gst::ElementFactory::find("curlhttpsrc").is_some(),
        gst_version: (major, minor, micro),
    }
}

/// Which audio tier a pipeline is playing. Both build sites already carry
/// `is_dash`, so nothing re-sniffs the URI — one source of truth.
fn capability_of(is_dash: bool) -> crate::proxy::Capability {
    if is_dash {
        crate::proxy::Capability::Dash
    } else {
        crate::proxy::Capability::Lossy
    }
}

/// The audio thread's proxy decision, held as settings plus capabilities so a
/// refusal stays a refusal.
///
/// Deliberately NOT a stored `Route`. A `Route` has two states and the answer
/// has three: proxied, direct, and refused. Collapsing the third into
/// `Route::NoProxy` is the defect the spec's stage 1a calls out by name, and
/// it would let a blocked tier stream on the user's own address.
#[derive(Clone)]
struct AudioProxy {
    settings: crate::ProxySettings,
    caps: crate::proxy::HostCaps,
    /// A bypass list that was in the environment at launch and stayed there.
    /// Read once from the process-global recorded in `main.rs`, then carried
    /// per-instance so the tests can set it without touching a `OnceLock`.
    launch_bypass: bool,
}

impl AudioProxy {
    fn new(settings: crate::ProxySettings, caps: crate::proxy::HostCaps) -> Self {
        Self {
            settings,
            caps,
            launch_bypass: crate::proxy::launch_bypass_was_set(),
        }
    }

    /// Test-only override. Production always takes the launch-time value;
    /// nothing may call `remember_launch_bypass` from a test, because it is a
    /// `OnceLock` shared by every test in the process and the first call wins
    /// for all of them.
    ///
    /// Compiled unconditionally rather than gated on the test cfg: that
    /// attribute is the anchor `audio_rs_production_source` splits this file
    /// on, and a second one partway up would move the boundary here and
    /// silently stop guarding every line below it. The attribute below keeps
    /// the dead-code check live in test builds, where the method is used.
    #[cfg_attr(not(test), allow(dead_code))]
    fn with_launch_bypass(mut self, v: bool) -> Self {
        self.launch_bypass = v;
        self
    }

    fn route_for(
        &self,
        c: crate::proxy::Capability,
    ) -> Result<crate::proxy::Route, crate::proxy::BlockReason> {
        let plan = crate::proxy::plan(&self.settings, &self.caps)
            .map_err(|e| crate::proxy::BlockReason::new(e.to_string()))?;

        // After the `Direct` check, never before it. `Direct` hands routing
        // back to the system, bypass list included, so a user with no proxy
        // configured and an ambient `no_proxy` must keep playing.
        //
        // The mechanism this guards against is `curlhttpsrc`: it reads
        // `no_proxy` when the element is constructed and forwards it as
        // CURLOPT_NOPROXY, which beats the `proxy` property we set, and the
        // variable cannot be removed once GTK's threads exist — so only a
        // restart clears it.
        //
        // The refusal is deliberately WIDER than that mechanism, and this is
        // the note that says so rather than a claim the two line up. Curl wins
        // source selection only while credentials are in play
        // (`promote_curl_source` restores its rank otherwise), and F6 records
        // `souphttpsrc` as immune to `no_proxy` — so a credential-free proxy
        // would in all likelihood route correctly through soup. It is refused
        // regardless, because nothing here verifies which factory will win at
        // build time: the rank is process-global, this module mutates it, and
        // the spec chose fail-closed over an argument about autoplugging.
        if self.launch_bypass && !matches!(plan, crate::proxy::ProxyPlan::Direct) {
            return Err(crate::proxy::BlockReason::new(
                "a proxy bypass list was set in the environment when SONE \
                 started; restart SONE to route audio through the proxy",
            ));
        }

        plan.route(c, &self.caps)
    }
}

/// Whether a settings change alters what the audio thread may do.
///
/// Compares the `Result` across **both** capabilities. Three reasons, each of
/// which broke an earlier draft:
///
/// 1. "no proxy" and "blocked" are different answers that a bare `Route` cannot
///    tell apart, and the transition between them must force a teardown.
/// 2. Below GStreamer 1.26.10 every credentialed proxy leaves `Lossy` permanently
///    `Err`, and `BlockReason` carries no host — so two different proxies produce
///    byte-identical refusals there. Comparing only `Lossy` would report "no
///    change" when the user switches proxies mid-track; the `Dash` arm is what
///    catches it.
/// 3. Checking both removes the need to know which tier is playing — the worker
///    does not retain one, and inventing it was how the draft went wrong.
///
/// Strictly conservative: a spurious teardown costs one rebuild at the saved
/// position, and nothing else.
fn audio_route_differs(before: &AudioProxy, after: &AudioProxy) -> bool {
    [
        crate::proxy::Capability::Lossy,
        crate::proxy::Capability::Dash,
    ]
    .into_iter()
    .any(|c| before.route_for(c) != after.route_for(c))
}

/// The HTTP source elements this application can configure. Anything else the
/// hook sees — the `data:` URI source for a manifest, decoders, queues — is
/// left alone, because `set_property` panics on a property an element lacks.
const HTTP_SOURCE_FACTORIES: [&str; 2] = ["curlhttpsrc", "souphttpsrc"];

/// Apply one `Route` to one HTTP source element.
fn apply_route_to_source(source: &gst::Element, route: &crate::proxy::Route) {
    let Some(factory) = source.factory().map(|f| f.name().to_string()) else {
        return;
    };
    if !HTTP_SOURCE_FACTORIES.contains(&factory.as_str()) {
        return;
    }

    // `NoProxy` means the system's own configuration applies; these elements
    // read the environment themselves, so setting nothing is correct here.
    let crate::proxy::Route::Via { uri, creds } = route else {
        return;
    };

    source.set_property("proxy", uri);

    if let Some(c) = creds {
        source.set_property("proxy-id", &c.user);
        source.set_property("proxy-pw", &c.pass);
    }

    if factory == "curlhttpsrc" {
        // Both are `gint` on this element — a `u32` panics. Its defaults of 0
        // and -1 mean a dead-but-reachable proxy never produces a bus error.
        source.set_property("timeout", 15i32);
        source.set_property("retries", 3i32);
    }
}

/// Watch a whole pipeline for HTTP sources and configure each as it appears.
///
/// Pipeline-level rather than per-`uridecodebin`: the gapless path adds a second
/// branch later, and a per-element hook was measured leaving that branch's
/// source direct. One track also yields more than one source — a manifest
/// source and a per-stream segment source — so this must not be one-shot.
///
/// Takes the `Route` by value: the route is decided once, before the pipeline
/// exists, so the handler never locks on the streaming thread (the spec measured
/// a 13x preroll cost for a mutex here).
///
/// The captured value is therefore a snapshot. A pipeline that outlives a
/// settings change keeps it until `SetProxySettings` tears that pipeline down
/// and rebuilds it — this hook does not re-point a live source, and nothing
/// here should be read as claiming it does.
fn watch_pipeline_sources(pipeline: &gst::Pipeline, route: crate::proxy::Route) {
    pipeline.connect_deep_element_added(move |_pipeline, _bin, element| {
        // Check the factory first: this fires for every element in the graph.
        let is_source = element
            .factory()
            .map(|f| HTTP_SOURCE_FACTORIES.contains(&f.name().as_str()))
            .unwrap_or(false);
        if is_source {
            apply_route_to_source(element, &route);
        }
    });
}

/// Whether the element GStreamer would autoplug for an https URI is one this
/// application knows how to point at a proxy.
///
/// `apply_route_to_source` skips any factory outside `HTTP_SOURCE_FACTORIES`,
/// and for a *source* "skipped" means "unproxied". The allowlist is exhaustive
/// on a stock system, but which factory wins is decided by process-global rank,
/// which this very module mutates. Rather than trust that, ask — and let the
/// build sites refuse when the answer is no.
fn http_source_is_configurable() -> bool {
    let Ok(element) =
        gst::Element::make_from_uri(gst::URIType::Src, "https://example.invalid/probe", None)
    else {
        return false;
    };
    element
        .factory()
        .map(|f| HTTP_SOURCE_FACTORIES.contains(&f.name().as_str()))
        .unwrap_or(false)
}

/// Prefer the curl source only while credentials are in play.
///
/// Not optional: the soup source never answers a proxy's authentication
/// challenge on a CONNECT tunnel, and every streamed segment is HTTPS.
fn promote_curl_source(route: &crate::proxy::Route, original: Option<gst::Rank>) {
    let Some(factory) = gst::ElementFactory::find("curlhttpsrc") else {
        return;
    };
    if matches!(route, crate::proxy::Route::Via { creds: Some(_), .. }) {
        factory.set_rank(gst::Rank::PRIMARY + 100);
    } else if let Some(rank) = original {
        factory.set_rank(rank);
    }
}

type Reply<T> = mpsc::Sender<T>;

#[derive(Debug, Clone, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// Serializes probes, including refresh requests already waiting on the same probe.
type DeviceProbeResult = (std::time::Instant, Result<Vec<AudioDevice>, String>);

#[derive(Default)]
pub struct AudioDeviceCache {
    result: Mutex<Option<DeviceProbeResult>>,
}

impl AudioDeviceCache {
    pub fn seed(&self, devices: Vec<AudioDevice>) {
        let mut state = self.result.lock().unwrap_or_else(|p| p.into_inner());
        if state.is_none() {
            *state = Some((std::time::Instant::now(), Ok(devices)));
        }
    }

    pub fn get(&self, force_refresh: bool) -> Result<Vec<AudioDevice>, String> {
        self.probe(force_refresh, list_alsa_devices)
    }

    fn probe(
        &self,
        force_refresh: bool,
        probe: impl FnOnce() -> Result<Vec<AudioDevice>, String>,
    ) -> Result<Vec<AudioDevice>, String> {
        self.probe_started(std::time::Instant::now(), force_refresh, probe)
    }

    fn probe_started(
        &self,
        started: std::time::Instant,
        force_refresh: bool,
        probe: impl FnOnce() -> Result<Vec<AudioDevice>, String>,
    ) -> Result<Vec<AudioDevice>, String> {
        let mut state = self.result.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((completed, result)) = state.as_ref() {
            if *completed >= started
                || (!force_refresh
                    && result.is_ok()
                    && completed.elapsed() < std::time::Duration::from_secs(30))
            {
                return result.clone();
            }
        }
        let result = probe();
        *state = Some((std::time::Instant::now(), result.clone()));
        result
    }
}

fn integer_depth(format: &str) -> Option<u8> {
    match format {
        "S16LE" => Some(16),
        "S24LE" | "S24_32LE" => Some(24),
        "S32LE" => Some(32),
        _ => None,
    }
}

/// Only validated integer widening/repacking and identical float are allowed.
fn sample_format_preserved(source: &str, target: &str) -> bool {
    match (integer_depth(source), integer_depth(target)) {
        (Some(from), Some(to)) => to >= from,
        _ => source == "F32LE" && target == source,
    }
}

fn samples_preserved(source: &PcmFormat, target: &PcmFormat) -> bool {
    source.sample_rate > 0
        && source.channels > 0
        && source.sample_rate == target.sample_rate
        && (source.channels == target.channels || (source.channels == 2 && target.channels > 2))
        && sample_format_preserved(&source.gst_format, &target.gst_format)
}

fn pick_lossless_format(source: &str, supported: &[String]) -> Option<String> {
    if supported.iter().any(|f| f == source) && sample_format_preserved(source, source) {
        return Some(source.to_owned());
    }
    supported
        .iter()
        .filter(|f| sample_format_preserved(source, f))
        .min_by_key(|f| integer_depth(f).unwrap_or(u8::MAX))
        .cloned()
}

// ── PCM types ──────────────────────────────────────────────────────────

/// Raw PCM chunk from GStreamer appsink
struct AudioChunk {
    data: Vec<u8>,
    format: PcmFormat,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct PcmFormat {
    sample_rate: u32,
    channels: u32,
    gst_format: String,
    bytes_per_sample: u32,
}

// NOTE (2b): the old `NextTrack` / `PendingAdvance` slot structs (used by the
// 2a `about-to-finish` machinery) were removed in 2b-A1.

/// 2b-A2: the prerolled next-track branch. Built on the attach executor thread
/// and stored in the shared `Arc<Mutex<Option<NextBinState>>>` that the worker
/// (dedup/replace/gating), the executor (attach/detach), and the notify
/// handler (2b-A3 advance) all read.
struct NextBinState {
    /// The legacy `uridecodebin` for the next track. Linked through
    /// `branch_queue` → `concat sink_1`. Owned here so detach can null + remove it.
    bin: gst::Element,
    /// The per-branch upstream queue (C1) decoupling this decoder from concat's
    /// gate so it pre-buffers while the current track plays.
    branch_queue: gst::Element,
    /// The URI this branch is decoding. Read on promotion so the worker's
    /// `current_uri` follows a gapless advance — without it a route change after
    /// an advance would re-issue the previous track.
    uri: String,
    track_id: u64,
    qid: String,
    // Read by HandleGaplessAdvance to apply gain + emit `track-advanced` on the switch.
    norm_gain: f64,
    replay_gain: f64,
    peak_amplitude: f64,
}

/// Jobs for the serialized attach/detach executor thread (C3). Pad-slot
/// operations on `concat` must never race, so they are all funneled through
/// this single thread's mpsc. The worker dispatches and returns immediately;
/// the executor does the blocking `pipeline.add` / `sync_state_with_parent` /
/// `set_state(Null)` work off the worker thread.
enum AttachJob {
    /// Build a second `uridecodebin → branch_queue → concat sink_1`, preroll it,
    /// and store the resulting `NextBinState` into the shared slot.
    Attach {
        pipeline: gst::Pipeline,
        concat: gst::Element,
        /// The `route_generation` the target pipeline was built under — i.e.
        /// the route its `watch_pipeline_sources` hook applies. The executor
        /// compares it against the current generation and refuses when they
        /// differ: the hook is what actually configures this branch's source,
        /// so a pipeline older than the route would fetch the next track on
        /// the route the user just replaced.
        build_generation: u64,
        uri: String,
        is_dash: bool,
        track_id: u64,
        qid: String,
        norm_gain: f64,
        replay_gain: f64,
        peak_amplitude: f64,
    },
    /// Tear down a specific bin (+ its branch queue): set Null, release the
    /// concat request pad whose peer is the queue, and remove from the pipeline.
    /// Captured `pipeline`/`concat` clones are Normal-only (per C5 — the worker
    /// never dispatches this on DirectAlsa).
    Detach {
        pipeline: gst::Pipeline,
        concat: gst::Element,
        bin: gst::Element,
        branch_queue: gst::Element,
    },
}

/// Commands to the ALSA writer thread
enum WriterCommand {
    Data(AudioChunk),
    EndOfTrack {
        emit_finished: bool,
        generation: u64,
    },
    FormatHint(PcmFormat),
    Resampling {
        from: u32,
        to: u32,
    },
    PendingPromotion {
        from: String,
        generation: u64,
    },
    Flush,
    /// Arm CamillaDSP for the next track. The same path and generation keep
    /// the live engine, so every PlayUrl can send this without reloading.
    #[cfg(target_os = "linux")]
    SetFir {
        path: Option<String>,
        generation: u64,
    },
    Shutdown,
}

/// Active playback backend — determines command dispatch.
/// The ALSA writer sender + thread handle live as separate state variables
/// so they persist across PlayUrl calls (track changes keep DAC open).
enum PlaybackBackend {
    /// Normal: full GStreamer pipeline with autoaudiosink.
    /// `concat` sits at the head (per-branch `queue` → concat → chain → sink)
    /// so the next track's decoder can preroll ahead for gapless (2b).
    Normal {
        pipeline: gst::Pipeline,
        concat: gst::Element,
        user_volume_el: Option<gst::Element>,
        norm_volume_el: Option<gst::Element>,
    },
    /// Exclusive/Bit-perfect: GStreamer decode → appsink, ALSA writer is external
    DirectAlsa {
        pipeline: gst::Pipeline,
        user_volume_el: Option<gst::Element>,
        norm_volume_el: Option<gst::Element>,
    },
    /// Decode to a sized localhost WAV. HQPlayer Desktop owns the DAC.
    HqPlayer {
        pipeline: gst::Pipeline,
        control: Arc<Mutex<Option<crate::hqplayer::ControlSession>>>,
        feed: crate::hqplayer::WavFeed,
        heard: Arc<AtomicU32>,
        finish_emit: Arc<AtomicBool>,
    },
}

impl PlaybackBackend {
    fn user_volume_el(&self) -> Option<&gst::Element> {
        match self {
            PlaybackBackend::Normal { user_volume_el, .. }
            | PlaybackBackend::DirectAlsa { user_volume_el, .. } => user_volume_el.as_ref(),
            PlaybackBackend::HqPlayer { .. } => None,
        }
    }

    fn norm_volume_el(&self) -> Option<&gst::Element> {
        match self {
            PlaybackBackend::Normal { norm_volume_el, .. }
            | PlaybackBackend::DirectAlsa { norm_volume_el, .. } => norm_volume_el.as_ref(),
            PlaybackBackend::HqPlayer { .. } => None,
        }
    }

    /// The backend's GStreamer pipeline. Both variants own a `gst::Pipeline`.
    #[allow(dead_code)]
    fn pipeline(&self) -> &gst::Pipeline {
        match self {
            PlaybackBackend::Normal { pipeline, .. }
            | PlaybackBackend::DirectAlsa { pipeline, .. }
            | PlaybackBackend::HqPlayer { pipeline, .. } => pipeline,
        }
    }

    /// The head `concat` element. Normal-only — gapless never runs on
    /// DirectAlsa (the mode gate prevents this path), so calling it on
    /// DirectAlsa is a programming error.
    #[allow(dead_code)]
    fn concat(&self) -> &gst::Element {
        match self {
            PlaybackBackend::Normal { concat, .. } => concat,
            PlaybackBackend::DirectAlsa { .. } => {
                panic!("PlaybackBackend::concat() called on DirectAlsa — gapless is normal-only")
            }
            PlaybackBackend::HqPlayer { .. } => {
                panic!("PlaybackBackend::concat() called on HqPlayer — gapless is normal-only")
            }
        }
    }
}

// ── Helper functions ───────────────────────────────────────────────────

fn parse_pcm_format(caps: &gst::CapsRef) -> Option<PcmFormat> {
    let s = caps.structure(0)?;
    if s.name() != "audio/x-raw" || !caps.is_fixed() {
        return None;
    }
    let format = s.get::<&str>("format").ok()?;
    let rate = s.get::<i32>("rate").ok()? as u32;
    let channels = s.get::<i32>("channels").ok()? as u32;
    if rate == 0 || rate > i32::MAX as u32 || channels == 0 || channels > i32::MAX as u32 {
        return None;
    }
    let bps = match format {
        "S16LE" => 2,
        "S24LE" => 3,
        "S24_32LE" | "S32LE" | "F32LE" => 4,
        other => {
            log::warn!("[audio] unsupported PCM format: {other}");
            return None;
        }
    };
    Some(PcmFormat {
        sample_rate: rate,
        channels,
        gst_format: format.to_string(),
        bytes_per_sample: bps,
    })
}

#[cfg(target_os = "linux")]
fn gst_format_to_alsa(gst_format: &str) -> alsa::pcm::Format {
    match gst_format {
        "S16LE" => alsa::pcm::Format::S16LE,
        "S24LE" => alsa::pcm::Format::S243LE,
        "S24_32LE" => alsa::pcm::Format::S24LE,
        "S32LE" => alsa::pcm::Format::S32LE,
        "F32LE" => alsa::pcm::Format::FloatLE,
        _ => alsa::pcm::Format::S32LE,
    }
}

#[cfg(target_os = "linux")]
fn alsa_format_to_gst(alsa_fmt: alsa::pcm::Format) -> (&'static str, u32) {
    // Inverse of gst_format_to_alsa. The ALSA/GStreamer 24-bit naming is swapped:
    //   ALSA S24LE  = 24-in-32 container = GStreamer S24_32LE (4 bytes/sample)
    //   ALSA S243LE = packed 24-bit       = GStreamer S24LE   (3 bytes/sample)
    match alsa_fmt {
        alsa::pcm::Format::S32LE => ("S32LE", 4),
        alsa::pcm::Format::S24LE => ("S24_32LE", 4),
        alsa::pcm::Format::S243LE => ("S24LE", 3),
        alsa::pcm::Format::S16LE => ("S16LE", 2),
        alsa::pcm::Format::FloatLE => ("F32LE", 4),
        _ => ("S32LE", 4),
    }
}

/// Converts perceptual linear volume (0.0 to 1.0 from the UI)
/// into an audio amplitude curve (cubic taper, ~50 dB range).
#[inline]
fn slider_to_amplitude(slider_val: f64) -> f64 {
    slider_val.clamp(0.0, 1.0).powi(3)
}

/// Triangular (TPDF) noise in (-1, 1), one LSB once it is added to an
/// integer sample. Two uniform draws summed and recentered.
struct TpdfDither {
    state: u64,
}

impl TpdfDither {
    fn new() -> Self {
        // Non-zero seed. The sequence only decorrelates requantization;
        // it is not a secret.
        Self {
            state: 0x5A17_E4D2_C0FF_EE01,
        }
    }

    fn uniform(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let u = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (u >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
    }

    fn triangular(&mut self) -> f64 {
        self.uniform() - self.uniform()
    }
}

/// Scale interleaved PCM by `gain`.
///
/// Unity gain returns without reading the samples, so bit-perfect playback at
/// slider 100% and ReplayGain 1 stays byte-identical. A product that already
/// lands on an integer is stored as that integer. Every other integer sample
/// gets one LSB of TPDF before it is rounded, which is what keeps attenuation
/// from turning into correlated quantization distortion. `S24_32LE` keeps its
/// 24 bits in the low three bytes (GStreamer 1.28); the high byte is the sign
/// extension. Float samples are multiplied and clamped, with no dither.
/// Gain 0 is digital silence, including a ragged tail, so mute does not leave
/// a dither floor.
fn apply_pcm_gain(data: &mut [u8], gst_format: &str, gain: f32, dither: &mut impl FnMut() -> f64) {
    if !gain.is_finite() || (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    // Exact zero, including -0.0. Any other finite gain still scales the samples.
    #[allow(clippy::float_cmp)]
    if gain == 0.0 {
        data.fill(0);
        return;
    }
    let gain_f = gain as f64;

    // f64's ulp around 2^31 is ~5e-7. Anything closer than 1e-4 to an integer
    // is the multiply landing on that integer, not a real fraction.
    const EXACT_EPS: f64 = 1e-4;

    let quantize = |sample: i64, lo: i64, hi: i64, dither: &mut dyn FnMut() -> f64| -> i64 {
        let scaled = sample as f64 * gain_f;
        let nearest = scaled.round();
        let quant = if (scaled - nearest).abs() < EXACT_EPS {
            nearest
        } else {
            (scaled + dither()).round()
        };
        quant.clamp(lo as f64, hi as f64) as i64
    };

    match gst_format {
        "S16LE" => {
            for chunk in data.as_chunks_mut::<2>().0 {
                let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                let v = quantize(s as i64, i16::MIN as i64, i16::MAX as i64, dither) as i16;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        "S32LE" => {
            for chunk in data.as_chunks_mut::<4>().0 {
                let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let v = quantize(s as i64, i32::MIN as i64, i32::MAX as i64, dither) as i32;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        "S24_32LE" => {
            for chunk in data.as_chunks_mut::<4>().0 {
                let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let v = quantize(s as i64, -8_388_608, 8_388_607, dither) as i32;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        "S24LE" => {
            for chunk in data.as_chunks_mut::<3>().0 {
                let raw = chunk[0] as i32 | (chunk[1] as i32) << 8 | (chunk[2] as i8 as i32) << 16;
                let v = quantize(raw as i64, -8_388_608, 8_388_607, dither) as i32;
                chunk[0] = v as u8;
                chunk[1] = (v >> 8) as u8;
                chunk[2] = (v >> 16) as u8;
            }
        }
        "F32LE" => {
            for chunk in data.as_chunks_mut::<4>().0 {
                let s = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let v = (s * gain).clamp(-1.0, 1.0);
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => {}
    }
}

/// Applies a normalization gain across all volume sinks: the GStreamer
/// `norm_vol` element (if present), the local `current_norm_gain` mirror, the
/// combined-volume atom (read by the ALSA writer), and the signal-path tracker.
/// Shared by `SetNormalizationGain` and the gapless `HandleGaplessAdvance` path.
fn apply_normalization_gain(
    gain: f64,
    current_norm_gain: &mut f64,
    norm_volume_el: Option<&gst::Element>,
    combined_vol: &Arc<AtomicU32>,
    current_volume: f64,
    signal_path: &SignalPathTracker,
) {
    *current_norm_gain = gain;
    if let Some(el) = norm_volume_el {
        el.set_property("volume", gain);
    }
    let amp = slider_to_amplitude(current_volume);
    combined_vol.store(((amp * gain) as f32).to_bits(), Ordering::Relaxed);
    signal_path.set_norm_gain_factor(gain as f32);
}

/// 2b-A2: build + preroll the next-track branch on the executor thread.
///
/// Mirrors the first branch's wiring (sink_0): legacy `uridecodebin` →
/// per-branch `queue` → `concat sink_1`. The branch queue (C1) decouples this
/// decoder from concat's back-pressure on the inactive sink pad so it
/// pre-buffers ahead while the current track plays. A smaller `buffer-duration`
/// (~3s, per C3) prerolls the source without fully pre-downloading it.
///
/// Returns the constructed elements so the caller can stash them in
/// `NextBinState`. On any error the partially-added elements are removed so the
/// pipeline isn't left with a dangling half-attached bin.
fn attach_next_bin(
    pipeline: &gst::Pipeline,
    concat: &gst::Element,
    uri: &str,
    is_dash: bool,
) -> Result<(gst::Element, gst::Element), String> {
    let udb = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        // 15s compressed buffer so the next track (and the current one once this
        // becomes active) rides out network jitter on slow connections. We have
        // the whole current track as lead time to fill it during preroll.
        .property("buffer-duration", 15_000_000_000i64)
        .property("use-buffering", true)
        .build()
        .map_err(|e| format!("Failed to create next uridecodebin: {e}"))?;
    // No route here: this bin joins a pipeline whose `watch_pipeline_sources`
    // hook already configures every HTTP source that appears under it.
    // Same props as the first branch's queue: 15s of decoded reservoir ahead of
    // concat — comfortable cushion against slow-internet rebuffering.
    let branch_queue = gst::ElementFactory::make("queue")
        .property("max-size-time", 15_000_000_000u64)
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .build()
        .map_err(|e| format!("Failed to create next branch queue: {e}"))?;

    if let Err(e) = pipeline.add_many([&udb, &branch_queue]) {
        return Err(format!("Failed to add next bin elements: {e}"));
    }

    // Link branch_queue.src → concat sink_1 (sink_0 is taken by the first
    // branch, so this request deterministically gets sink_1).
    let concat_sink = match concat.request_pad_simple("sink_%u") {
        Some(p) => p,
        None => {
            let _ = pipeline.remove_many([&udb, &branch_queue]);
            return Err("concat refused next sink pad".to_string());
        }
    };
    let queue_src = match branch_queue.static_pad("src") {
        Some(p) => p,
        None => {
            concat.release_request_pad(&concat_sink);
            let _ = pipeline.remove_many([&udb, &branch_queue]);
            return Err("next branch queue has no src pad".to_string());
        }
    };
    if let Err(e) = queue_src.link(&concat_sink) {
        concat.release_request_pad(&concat_sink);
        let _ = pipeline.remove_many([&udb, &branch_queue]);
        return Err(format!("Failed to link next queue→concat: {e}"));
    }

    // uridecodebin(B) → branch_queue (dynamic). Mirror sink_0's pad_added guard:
    // skip already-linked + non-audio pads.
    let branch_queue_weak = branch_queue.downgrade();
    udb.connect_pad_added(move |_src, src_pad| {
        let Some(branch_queue) = branch_queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = branch_queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if let Some(caps) = src_pad.current_caps() {
            if let Some(s) = caps.structure(0) {
                if !s.name().as_str().starts_with("audio/") {
                    return;
                }
            }
        }
        if let Err(e) = src_pad.link(&sink_pad) {
            log::error!("Failed to link next uridecodebin pad: {e:?}");
        }
    });

    // Preroll-before-PLAYING (fixes the `not-linked` race on async/network/DASH
    // sources). Going straight to PLAYING via `sync_state_with_parent` lets the
    // demuxer's streaming thread emit its decoded src pad AND push the first
    // buffer before the `pad_added` handler above links it into `branch_queue` —
    // the buffer hits an unlinked pad and `not-linked` (-1) propagates up to the
    // demuxer ("GstDashDemux: streaming stopped, reason not-linked"). Local files
    // decode instantly so the link always wins, masking the bug.
    //
    // Instead, bring B up to PAUSED only and BLOCK until the async preroll
    // settles (`get_state` returns once the state change is ASYNC-DONE). In
    // PAUSED no data flows past the prerolled pad, so `pad_added` fires and links
    // during preroll — guaranteeing the link is in place before any buffer moves.
    // Only then promote to PLAYING. `concat` back-pressures the inactive sink pad,
    // so the branch holds prerolled and switches in gap-free at sink_0's EOS.
    if let Err(e) = branch_queue.set_state(gst::State::Paused) {
        log::error!("next branch queue set Paused failed: {e}");
    }
    if let Err(e) = udb.set_state(gst::State::Paused) {
        log::error!("next uridecodebin set Paused failed: {e}");
    }
    // Wait for the preroll to complete so pad_added has fired + linked. Bounded
    // so a stalled network source can't hang the executor thread.
    let (ret, cur, pend) = udb.state(gst::ClockTime::from_seconds(15));
    log::debug!("[audio] gapless: next bin preroll state ret={ret:?} cur={cur:?} pend={pend:?}");

    // Preroll done + pad linked: now safe to promote to PLAYING.
    if let Err(e) = branch_queue.sync_state_with_parent() {
        log::error!("next branch queue sync_state failed: {e}");
    }
    if let Err(e) = udb.sync_state_with_parent() {
        log::error!("next uridecodebin sync_state failed: {e}");
    }
    log::debug!("[audio] gapless: attached next bin (is_dash={is_dash})");

    Ok((udb, branch_queue))
}

/// 2b-A2: tear down a next-track branch on the executor thread.
///
/// Sets the bin + its branch queue to Null, finds the `concat` sink pad whose
/// peer's parent is this bin's queue, unlinks + releases that request pad, then
/// removes both elements from the pipeline. Only ever called with Normal-mode
/// `pipeline`/`concat` clones (the worker gates dispatch — never DirectAlsa, C5).
fn detach_bin(
    pipeline: &gst::Pipeline,
    concat: &gst::Element,
    bin: &gst::Element,
    branch_queue: &gst::Element,
) {
    // Order matters. Unlink + release the concat request pad FIRST, BEFORE
    // nulling the bin. The branch queue's src is linked to concat's INACTIVE sink
    // pad, which concat hard-blocks (it only pulls from the active pad). If we
    // null the bin while still linked, its streaming threads are stuck pushing
    // into that blocked pad and the NULL transition can't complete — the
    // `set_state(Null)` call blocks the executor thread indefinitely on a live
    // network source. Releasing the pad first unblocks them so NULL completes.
    let sink_pads: Vec<gst::Pad> = concat
        .sink_pads()
        .into_iter()
        .filter(|pad| {
            pad.peer()
                .and_then(|peer| peer.parent_element())
                .is_some_and(|parent| &parent == branch_queue)
        })
        .collect();
    for pad in sink_pads {
        if let Some(peer) = pad.peer() {
            let _ = peer.unlink(&pad);
        }
        concat.release_request_pad(&pad);
    }

    // Now drive both elements to NULL and BLOCK until the (possibly async)
    // transition actually completes. Removing/dropping an element still mid-
    // transition disposes it in a non-NULL state, which emits GStreamer
    // CRITICALs ("Trying to dispose element … in PLAYING instead of the NULL
    // state") + GST_IS_ELEMENT assertion failures. `get_state` with a bounded
    // timeout guarantees we only `remove_many` once both are truly NULL.
    let _ = bin.set_state(gst::State::Null);
    let _ = branch_queue.set_state(gst::State::Null);
    let (br, bcur, _) = bin.state(gst::ClockTime::from_seconds(10));
    let (qr, qcur, _) = branch_queue.state(gst::ClockTime::from_seconds(10));
    log::debug!("[audio] gapless: next bin NULL wait bin={br:?}/{bcur:?} queue={qr:?}/{qcur:?}");

    let _ = pipeline.remove_many([bin, branch_queue]);
    log::debug!("[audio] gapless: detached next bin");
}

/// 2b-A2: the serialized attach/detach executor loop (C3). One dedicated thread
/// owns this so pad-slot operations on `concat` are strictly ordered and never
/// block the worker command thread. `next_bin` is the shared slot the worker /
/// executor / notify handler (2b-A3) all read.
fn run_attach_executor(
    job_rx: mpsc::Receiver<AttachJob>,
    next_bin: Arc<Mutex<Option<NextBinState>>>,
    audio_proxy: Arc<Mutex<AudioProxy>>,
    route_generation: Arc<AtomicU64>,
    hq_enabled: Arc<AtomicBool>,
) {
    for job in job_rx {
        match job {
            AttachJob::Attach {
                pipeline,
                concat,
                build_generation,
                uri,
                is_dash,
                track_id,
                qid,
                norm_gain,
                replay_gain,
                peak_amplitude,
            } => {
                // Snapshot before the route is even read: if a settings change
                // lands from here on, this branch is built under a route that is
                // no longer current and must not be armed.
                let generation_at_start = route_generation.load(Ordering::Acquire);
                if hq_enabled.load(Ordering::Acquire) {
                    log::debug!("[hqplayer] skipping gapless preroll");
                    continue;
                }
                // The route below is recomputed from the current settings, but
                // it is not what configures this branch: the target pipeline's
                // hook is, and that hook holds the route of `build_generation`.
                // Refuse before `attach_next_bin`, because that call prerolls —
                // up to fifteen seconds of the next track on the old route.
                //
                // Nothing to detach here: this job built nothing, and the slot
                // it would otherwise clear belongs to the pipeline that is now
                // current (`SetProxySettings` already detached anything left on
                // the stale one), so dropping that reference without detaching
                // would strand a branch already linked to concat.
                if build_generation != generation_at_start {
                    log::warn!(
                        "[proxy] refusing to preroll onto a pipeline built under the previous route"
                    );
                    continue;
                }
                let route = {
                    let ap = audio_proxy.lock().unwrap_or_else(|p| p.into_inner());
                    ap.route_for(capability_of(is_dash))
                };
                if let Err(blocked) = route {
                    // A branch we may not proxy is a branch we must not preroll.
                    log::warn!("[proxy] refusing to preroll next track: {}", blocked.cause);
                    if let Ok(mut guard) = next_bin.lock() {
                        *guard = None;
                    }
                    continue;
                }

                match attach_next_bin(&pipeline, &concat, &uri, is_dash) {
                    Ok((bin, branch_queue)) => {
                        // Re-read the generation and store under ONE hold of the
                        // `next_bin` mutex, the same one `SetProxySettings` bumps
                        // and takes under. Two independent synchronisation points
                        // leave this legal interleaving: the settings change takes
                        // an empty slot (detaching nothing) and this thread then
                        // stores a branch built under the route it just replaced.
                        let mut guard = match next_bin.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        if route_generation.load(Ordering::Acquire) != generation_at_start
                            || hq_enabled.load(Ordering::Acquire)
                        {
                            // The branch is already in the pipeline and linked to
                            // concat's sink_1, so concat would switch to it at the
                            // boundary whether or not this slot names it. Dropping
                            // the reference is not enough — it has to be detached.
                            drop(guard);
                            log::warn!(
                                "[audio] discarding a next branch that must not become current"
                            );
                            detach_bin(&pipeline, &concat, &bin, &branch_queue);
                            continue;
                        }
                        *guard = Some(NextBinState {
                            bin,
                            branch_queue,
                            uri,
                            track_id,
                            qid,
                            norm_gain,
                            replay_gain,
                            peak_amplitude,
                        });
                    }
                    Err(e) => {
                        // Preload failure is non-fatal: leave the slot empty so
                        // the natural track boundary falls back to playNext.
                        log::warn!("[audio] gapless: attach_next_bin failed: {e}");
                        if let Ok(mut guard) = next_bin.lock() {
                            *guard = None;
                        }
                    }
                }
            }
            AttachJob::Detach {
                pipeline,
                concat,
                bin,
                branch_queue,
            } => {
                detach_bin(&pipeline, &concat, &bin, &branch_queue);
            }
        }
    }
}

/// Probe which GStreamer format strings an ALSA device supports.
/// Returns a list like `["S32LE", "S24_32LE", "S16LE"]`.
#[cfg(target_os = "linux")]
fn probe_supported_gst_formats(pcm: &alsa::PCM) -> Vec<&'static str> {
    use alsa::pcm::{Format, HwParams};

    let Ok(hwp) = HwParams::any(pcm) else {
        return vec!["S32LE"]; // safe fallback
    };
    let probe: &[(Format, &str)] = &[
        (Format::S32LE, "S32LE"),
        (Format::S24LE, "S24_32LE"), // ALSA S24LE = GStreamer S24_32LE
        (Format::S243LE, "S24LE"),   // ALSA S243LE = GStreamer S24LE
        (Format::FloatLE, "F32LE"),
        (Format::S16LE, "S16LE"),
    ];
    let supported: Vec<&str> = probe
        .iter()
        .filter(|(f, _)| hwp.test_format(*f).is_ok())
        .map(|(_, name)| *name)
        .collect();
    if supported.is_empty() {
        vec!["S32LE"] // safe fallback
    } else {
        supported
    }
}

/// Pick a compatible capsfilter format outside strict bit-perfect mode.
/// Priority:
///   1. Pass-through if the DAC supports the source format directly (zero conversion work).
///   2. Narrowest lossless promotion the DAC supports (container widening or, for S24_32LE,
///      shrinking to S24LE which holds the same 24 audio bits in 3 bytes).
///   3. Lossy fallback: DAC's first probed format (widest per probe order).
///
/// This fallback is only used outside strict bit-perfect mode.
#[cfg(target_os = "linux")]
fn pick_capsfilter_format(source: &str, dac_supported: &[String]) -> String {
    // 1. Pass-through.
    if dac_supported.iter().any(|f| f == source) {
        return source.to_string();
    }
    // 2. Narrowest lossless promotion. audioconvert with dithering=none does pure
    //    integer bit-shift conversions between these formats — no quantization.
    //    S24_32LE → S24LE strips the unused high byte and preserves the 24 audio
    //    bits exactly.
    let promotions: &[&str] = match source {
        "S16LE" => &["S24LE", "S24_32LE", "S32LE"],
        "S24LE" => &["S24_32LE", "S32LE"],
        "S24_32LE" => &["S24LE", "S32LE"], // S24LE = same 24 bits, narrower container
        _ => &[], // S32LE, F32LE, unknowns: no lossless integer alternative
    };
    if let Some(p) = promotions
        .iter()
        .find(|p| dac_supported.iter().any(|f| f == *p))
    {
        return (*p).to_string();
    }
    // 3. Compatibility fallback — DAC's preferred (widest) format.
    dac_supported
        .first()
        .cloned()
        .unwrap_or_else(|| "S32LE".to_string())
}

/// Probe which standard sample rates an ALSA device supports.
/// Tests common audiophile rates and returns those that pass.
#[cfg(target_os = "linux")]
fn probe_supported_rates(pcm: &alsa::PCM) -> Vec<u32> {
    use alsa::pcm::HwParams;

    let Ok(hwp) = HwParams::any(pcm) else {
        return vec![44100, 48000]; // safe fallback
    };
    let candidates: &[u32] = &[
        44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000,
    ];
    let supported: Vec<u32> = candidates
        .iter()
        .copied()
        .filter(|&r| hwp.test_rate(r).is_ok())
        .collect();
    if supported.is_empty() {
        vec![44100, 48000] // safe fallback
    } else {
        supported
    }
}

const PCM_WAIT_MS: u32 = 50;
const PCM_STALL: std::time::Duration = std::time::Duration::from_secs(2);

trait PcmIo {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, i32>;
    fn wait(&mut self, millis: u32) -> Result<(), i32>;
    fn resume(&mut self) -> Result<(), i32>;
    fn prepare(&mut self) -> Result<(), i32>;
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn sleep(&mut self, millis: u32) {
        std::thread::sleep(std::time::Duration::from_millis(millis.into()));
    }
}

#[cfg(target_os = "linux")]
struct AlsaIo<'a>(&'a alsa::PCM);
#[cfg(target_os = "linux")]
impl PcmIo for AlsaIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, i32> {
        self.0.io_bytes().writei(bytes).map_err(|e| e.errno())
    }
    fn wait(&mut self, millis: u32) -> Result<(), i32> {
        self.0.wait(Some(millis)).map(|_| ()).map_err(|e| e.errno())
    }
    fn resume(&mut self) -> Result<(), i32> {
        self.0.resume().map_err(|e| e.errno())
    }
    fn prepare(&mut self) -> Result<(), i32> {
        self.0.prepare().map_err(|e| e.errno())
    }
}

fn pcm_error(errno: i32) -> &'static str {
    if errno == libc::ENODEV {
        "device_disconnected"
    } else {
        "write_error"
    }
}

fn recover_pcm(
    io: &mut impl PcmIo,
    errno: i32,
    cancelled: &AtomicBool,
) -> Result<(), &'static str> {
    if cancelled.load(Ordering::Acquire) {
        return Err("cancelled");
    }
    if errno == libc::EPIPE {
        return io.prepare().map_err(pcm_error);
    }
    if errno != libc::ESTRPIPE {
        return Err(pcm_error(errno));
    }
    let deadline = io.now() + PCM_STALL;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err("cancelled");
        }
        match io.resume() {
            Ok(()) => return Ok(()),
            Err(libc::EAGAIN) if io.now() < deadline => io.sleep(20),
            Err(libc::EAGAIN) => return Err("suspend_timeout"),
            Err(libc::ENODEV) => return Err("device_disconnected"),
            Err(_) => return io.prepare().map_err(pcm_error),
        }
    }
}

/// Offset advances only for accepted frames. The deadline covers pending audio,
/// excludes user pause, and resets only after actual progress.
fn write_pcm(
    io: &mut impl PcmIo,
    data: &[u8],
    frame_size: usize,
    cancelled: &AtomicBool,
    paused: &AtomicBool,
    frames_written: &AtomicU64,
) -> Result<(), &'static str> {
    if frame_size == 0 || !data.len().is_multiple_of(frame_size) {
        return Err("invalid_pcm");
    }
    let mut offset = 0;
    let mut progress = io.now();
    while offset < data.len() {
        if cancelled.load(Ordering::Acquire) {
            return Err("cancelled");
        }
        if paused.load(Ordering::Acquire) {
            io.sleep(20);
            progress = io.now();
            continue;
        }
        if io.now().duration_since(progress) >= PCM_STALL {
            return Err("write_timeout");
        }
        match io.write(&data[offset..]) {
            Ok(n) if n > 0 => {
                offset += n * frame_size;
                frames_written.fetch_add(n as u64, Ordering::Relaxed);
                progress = io.now();
            }
            Ok(_) | Err(libc::EAGAIN) => {
                if let Err(e) = io.wait(PCM_WAIT_MS) {
                    recover_pcm(io, e, cancelled)?;
                }
            }
            Err(e) => recover_pcm(io, e, cancelled)?,
        }
    }
    Ok(())
}

// ── ALSA writer thread ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn configure_alsa_hwparams(
    pcm: &alsa::PCM,
    fmt: &PcmFormat,
    bit_perfect: bool,
    exact_rate: bool,
) -> Result<PcmFormat, String> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::ValueOr;

    let hwp = HwParams::any(pcm).map_err(|e| format!("HwParams::any failed: {e}"))?;
    hwp.set_access(Access::RWInterleaved)
        .map_err(|e| format!("set_access: {e}"))?;

    // Probe and log all supported formats
    let probe_formats: &[(Format, &str)] = &[
        (Format::S32LE, "S32LE (32-bit)"),
        (Format::S24LE, "S24LE (24-in-32)"),
        (Format::S243LE, "S24_3LE (24-bit packed)"),
        (Format::FloatLE, "F32LE (float)"),
        (Format::S16LE, "S16LE (16-bit)"),
    ];
    let supported: Vec<&str> = probe_formats
        .iter()
        .filter(|(f, _)| hwp.test_format(*f).is_ok())
        .map(|(_, name)| *name)
        .collect();
    log::debug!("[audio] DAC supported formats: [{}]", supported.join(", "));

    let requested = gst_format_to_alsa(&fmt.gst_format);

    let alsa_fmt = if bit_perfect {
        hwp.set_format(requested)
            .map_err(|e| format!("set_format({}): {e}", fmt.gst_format))?;
        requested
    } else {
        // Ranked fallback: requested first, then descending quality
        let fallbacks: &[Format] = &[
            Format::S32LE,
            Format::S24LE,  // 24-in-32 container
            Format::S243LE, // 24-bit packed
            Format::FloatLE,
            Format::S16LE,
        ];
        let mut candidates: Vec<Format> = Vec::with_capacity(6);
        candidates.push(requested);
        for &f in fallbacks {
            if f != requested {
                candidates.push(f);
            }
        }
        let mut chosen = None;
        for &candidate in &candidates {
            if hwp.test_format(candidate).is_ok() {
                hwp.set_format(candidate)
                    .map_err(|e| format!("set_format after test: {e}"))?;
                chosen = Some(candidate);
                break;
            }
        }
        chosen.ok_or_else(|| {
            "Audio device does not support any compatible sample format".to_string()
        })?
    };

    let lock_rate = bit_perfect || exact_rate;
    if lock_rate {
        hwp.set_rate_resample(false)
            .map_err(|e| format!("set_rate_resample: {e}"))?;
    }
    hwp.set_rate(fmt.sample_rate, ValueOr::Nearest)
        .map_err(|e| {
            if lock_rate {
                log::warn!("[audio] set_rate({}) failed: {e}", fmt.sample_rate);
                format!(
                    "Audio device cannot preserve source rate {} Hz",
                    fmt.sample_rate
                )
            } else {
                format!("set_rate({}): {e}", fmt.sample_rate)
            }
        })?;
    if lock_rate {
        let actual_rate = hwp.get_rate().map_err(|e| format!("get_rate: {e}"))?;
        if actual_rate != fmt.sample_rate {
            log::warn!(
                "[audio] rate mismatch: DAC negotiated {}Hz, track requires {}Hz",
                actual_rate,
                fmt.sample_rate
            );
            return Err(format!(
                "Audio device cannot preserve source rate {} Hz",
                fmt.sample_rate
            ));
        }
    }
    // Negotiate channel count. Some DACs (USB pro interfaces like Focusrite /
    // Audient) expose only a fixed channel count and reject 2ch stereo. Test the
    // requested count; if unsupported, fall back to the device's native minimum.
    let hw_channels = if hwp.test_channels(fmt.channels).is_ok() {
        fmt.channels
    } else if bit_perfect {
        return Err(format!(
            "Bit-perfect output cannot change {} channels",
            fmt.channels
        ));
    } else {
        match hwp.get_channels_min() {
            Ok(n) if n > 0 => {
                log::info!(
                    "[audio] DAC rejects {}ch, using device-native {}ch",
                    fmt.channels,
                    n
                );
                n
            }
            _ => {
                return Err(format!(
                    "DAC rejects {}ch and exposes no usable channel count",
                    fmt.channels
                ))
            }
        }
    };
    hwp.set_channels(hw_channels)
        .map_err(|e| format!("set_channels({hw_channels}): {e}"))?;
    hwp.set_buffer_time_near(500_000, ValueOr::Nearest)
        .map_err(|e| format!("set_buffer_time: {e}"))?;
    hwp.set_period_time_near(50_000, ValueOr::Nearest)
        .map_err(|e| format!("set_period_time: {e}"))?;
    pcm.hw_params(&hwp).map_err(|e| format!("hw_params: {e}"))?;

    // Configure sw_params: pre-fill buffer before DMA starts.
    // snd_pcm_hw_params() resets start_threshold to 1 (immediate start on first
    // writei), which causes underruns when the writer can't keep up from frame one.
    // Match GStreamer alsasink: start_threshold = buffer_size (full pre-fill).
    {
        let swp = pcm
            .sw_params_current()
            .map_err(|e| format!("sw_params_current: {e}"))?;
        let hwp_active = pcm
            .hw_params_current()
            .map_err(|e| format!("hw_params_current for sw: {e}"))?;
        let buffer_frames = hwp_active
            .get_buffer_size()
            .map_err(|e| format!("get_buffer_size: {e}"))?;
        let period_frames = hwp_active
            .get_period_size()
            .map_err(|e| format!("get_period_size: {e}"))?;
        // start_threshold: largest period-aligned value ≤ buffer_size.
        // With our time-near requests this equals buffer_size, but the
        // rounding guards against odd driver negotiations.
        let start = (buffer_frames / period_frames) * period_frames;
        swp.set_start_threshold(start as alsa::pcm::Frames)
            .map_err(|e| format!("set_start_threshold: {e}"))?;
        swp.set_avail_min(period_frames as alsa::pcm::Frames)
            .map_err(|e| format!("set_avail_min: {e}"))?;
        pcm.sw_params(&swp).map_err(|e| format!("sw_params: {e}"))?;
        log::debug!(
            "[audio] sw_params committed: start_threshold={}, avail_min={}",
            start,
            period_frames
        );
    }

    // Log final negotiated hw_params
    if let Ok(active) = pcm.hw_params_current() {
        let rate = active.get_rate().unwrap_or(0);
        let channels = active.get_channels().unwrap_or(0);
        let buffer_frames = active.get_buffer_size().unwrap_or(0);
        let period_frames = active.get_period_size().unwrap_or(0);
        log::debug!(
            "[audio] hw_params committed: rate={}Hz, channels={}, buffer={} frames, period={} frames",
            rate, channels, buffer_frames, period_frames
        );
    }

    let (gst_fmt_str, bps) = alsa_format_to_gst(alsa_fmt);
    if alsa_fmt != requested {
        log::info!(
            "[audio] format fallback: {} -> {} (DAC doesn't support {})",
            fmt.gst_format,
            gst_fmt_str,
            fmt.gst_format
        );
    }
    let actual_rate = pcm
        .hw_params_current()
        .and_then(|p| p.get_rate())
        .unwrap_or(fmt.sample_rate);
    Ok(PcmFormat {
        sample_rate: actual_rate,
        channels: hw_channels,
        gst_format: gst_fmt_str.to_string(),
        bytes_per_sample: bps,
    })
}

#[cfg(target_os = "linux")]
struct AlsaWriterConfig<'a> {
    device: &'a str,
    initial_format: &'a PcmFormat,
    app_handle: tauri::AppHandle,
    tearing_down: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    frames_written: Arc<AtomicU64>,
    current_sample_rate: Arc<AtomicU32>,
    writer_gen: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    bit_perfect: bool,
    preserve_rate: bool,
    combined_vol: Arc<AtomicU32>,
    signal_path: Arc<SignalPathTracker>,
    decoded_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    output_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
}

#[cfg(target_os = "linux")]
type AlsaWriterParts = (
    crossbeam_channel::Sender<WriterCommand>,
    JoinHandle<()>,
    PcmFormat,
    Vec<&'static str>,
    Vec<u32>,
);

#[cfg(target_os = "linux")]
fn spawn_alsa_writer(config: AlsaWriterConfig<'_>) -> Result<AlsaWriterParts, String> {
    let AlsaWriterConfig {
        device,
        initial_format,
        app_handle,
        tearing_down,
        cancelled,
        frames_written,
        current_sample_rate,
        writer_gen,
        paused,
        bit_perfect,
        preserve_rate,
        combined_vol,
        signal_path,
        decoded_cell,
        output_cell,
    } = config;
    let device = device.to_string();
    let initial_format = initial_format.clone();
    let (tx, rx) = crossbeam_channel::bounded::<WriterCommand>(256);

    // Open device eagerly to detect EBUSY immediately
    let pcm = alsa::PCM::new(&device, alsa::Direction::Playback, true).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("busy") || msg.contains("EBUSY") {
            "device_busy".to_string()
        } else {
            format!("Failed to open ALSA device: {e}")
        }
    })?;

    let supported_gst_formats = probe_supported_gst_formats(&pcm);
    log::debug!(
        "[alsa-writer] DAC supported GStreamer formats: {:?}",
        supported_gst_formats
    );

    let supported_rates = probe_supported_rates(&pcm);
    log::debug!("[alsa-writer] DAC supported rates: {:?}", supported_rates);

    // Adjust initial format to something the DAC actually supports.
    // This is a placeholder — the real format arrives via FormatHint/pad_added
    // once GStreamer decodes the stream. We just need the DAC to accept it.
    let initial_format = {
        let mut fmt = initial_format;
        if !supported_gst_formats.contains(&fmt.gst_format.as_str()) {
            let best = supported_gst_formats[0]; // probe orders by quality (S32>S24>S16)
            let (_, bps) = alsa_format_to_gst(gst_format_to_alsa(best));
            log::info!(
                "[alsa-writer] DAC doesn't support {}, using {} for initial config",
                fmt.gst_format,
                best
            );
            fmt.gst_format = best.to_string();
            fmt.bytes_per_sample = bps;
        }
        if !supported_rates.is_empty() && !supported_rates.contains(&fmt.sample_rate) {
            let fallback = supported_rates[0]; // first probed rate (44100 typically)
            log::info!(
                "[alsa-writer] DAC doesn't support {}Hz, using {}Hz for initial config",
                fmt.sample_rate,
                fallback
            );
            fmt.sample_rate = fallback;
        }
        fmt
    };

    let requested_for_fallback = initial_format.clone();
    let initial_format = configure_alsa_hwparams(&pcm, &initial_format, false, false)?;
    pcm.prepare().map_err(|e| format!("pcm.prepare: {e}"))?;
    current_sample_rate.store(initial_format.sample_rate, Ordering::Relaxed);
    let negotiated_fmt = initial_format.clone();

    signal_path.set_output(
        &initial_format.gst_format,
        initial_format.sample_rate,
        initial_format.channels,
    );
    if !bit_perfect && requested_for_fallback.gst_format != initial_format.gst_format {
        signal_path.record_format_fallback(
            &requested_for_fallback.gst_format,
            &initial_format.gst_format,
        );
    }

    let signal_path_thread = Arc::clone(&signal_path);
    let handle = std::thread::Builder::new()
        .name("alsa-writer".into())
        .spawn(move || {
            let sp = signal_path_thread;
            let mut pcm = pcm; // rebind as mutable for format-change reopen
            let mut current_fmt = initial_format;
            let period_duration = std::time::Duration::from_millis(50);

            let silence_frames = (current_fmt.sample_rate as usize * 50) / 1000;
            let mut silence_buf = vec![0u8; silence_frames * current_fmt.channels as usize * current_fmt.bytes_per_sample as usize];
            let mut tpdf = TpdfDither::new();

            // Bit-perfect promotion announcement: pad_added sends only the source format,
            // writer emits the toast once the actually-negotiated `current_fmt` is known.
            let mut pending_promotion_from: Option<String> = None;
            let resolve_pending = |pending: &mut Option<String>, current: &PcmFormat| {
                if let Some(from) = pending.take() {
                    if from != current.gst_format && sample_format_preserved(&from, &current.gst_format) {
                        log::info!("[alsa-writer] bit-depth promotion: {from} -> {}", current.gst_format);
                        sp.record_bit_depth_promotion(&from, &current.gst_format);
                        app_handle.emit(
                            "audio-bit-depth-changed",
                            serde_json::json!({ "from": from, "to": current.gst_format }),
                        ).ok();
                    } else {
                        log::debug!("[alsa-writer] bit-perfect: no promotion needed ({from})");
                    }
                }
            };

            let write_bytes = |pcm: &alsa::PCM, data: &[u8], fmt: &PcmFormat, fw: &AtomicU64, _silence: &[u8]| {
                write_pcm(&mut AlsaIo(pcm), data,
                    fmt.channels as usize * fmt.bytes_per_sample as usize,
                    &cancelled, &paused, fw)
            };
            let write_silence = |pcm: &alsa::PCM, buf: &[u8]| -> Result<(), &'static str> {
                if cancelled.load(Ordering::Acquire) { return Err("cancelled"); }
                let mut io = AlsaIo(pcm);
                match io.write(buf) {
                    Ok(_) | Err(libc::EAGAIN) => {
                        io.wait(PCM_WAIT_MS).or_else(|e| if e == libc::EAGAIN { Ok(()) } else { Err(e) }).map_err(pcm_error)
                    }
                    Err(e) => recover_pcm(&mut io, e, &cancelled),
                }
            };

            fn report_fir_failure(app_handle: &tauri::AppHandle, message: &str) {
                log::warn!("[camilla-fir] failed: {message}");
                app_handle
                    .emit(
                        "camilla-fir-status",
                        serde_json::json!({ "message": message }),
                    )
                    .ok();
            }

            fn mark_fir(_fir: &mut FirSlot, fir_live: &mut bool, signal_path: &SignalPathTracker, app_handle: &tauri::AppHandle, failed: Option<&str>) {
                if let Some(message) = failed {
                    if *fir_live {
                        *fir_live = false;
                        signal_path.set_camilla_fir(false);
                    }
                    report_fir_failure(app_handle, message);
                    return;
                }
                // A track start clears the snapshot without telling this
                // thread. Publish every chunk and let the tracker drop duplicates.
                signal_path.set_camilla_fir(true);
                *fir_live = true;
            }

            /// CamillaDSP, then the byte path. Volume dither stays in
            /// `apply_pcm_gain` when Camilla is off. An empty `Processed`
            /// buffer is held inside Camilla.
            #[allow(clippy::too_many_arguments)]
            fn play_chunk(
                pcm: &alsa::PCM,
                data: &mut [u8],
                source: &PcmFormat,
                device: &PcmFormat,
                vol: f32,
                dither: &mut TpdfDither,
                fir: &mut FirSlot,
                fir_live: &mut bool,
                signal_path: &SignalPathTracker,
                app_handle: &tauri::AppHandle,
                frames_written: &AtomicU64,
                _silence_buf: &[u8],
                cancelled: &AtomicBool,
                paused: &AtomicBool,
            ) -> Result<(), &'static str> {
                if source.gst_format != device.gst_format || source.channels != device.channels {
                    log::error!(
                        "[alsa-writer] PCM layout {}/{}ch does not match the device {}/{}ch",
                        source.gst_format,
                        source.channels,
                        device.gst_format,
                        device.channels
                    );
                    return Err("write_error");
                }
                let output = fir.render(
                    data,
                    source.sample_rate,
                    source.channels,
                    &source.gst_format,
                    vol,
                    &mut || dither.triangular(),
                );
                match output {
                    FirOutput::Off => {
                        if *fir_live {
                            *fir_live = false;
                            signal_path.set_camilla_fir(false);
                        }
                        apply_pcm_gain(data, &source.gst_format, vol, &mut || dither.triangular());
                        write_pcm(&mut AlsaIo(pcm), data, device.channels as usize * device.bytes_per_sample as usize, cancelled, paused, frames_written)
                    }
                    FirOutput::Processed(bytes) => {
                        mark_fir(fir, fir_live, signal_path, app_handle, None);
                        if bytes.is_empty() {
                            Ok(())
                        } else {
                            write_pcm(&mut AlsaIo(pcm), &bytes, device.channels as usize * device.bytes_per_sample as usize, cancelled, paused, frames_written)
                        }
                    }
                    FirOutput::Failed(message) => {
                        mark_fir(fir, fir_live, signal_path, app_handle, Some(&message));
                        Err("dsp_processing_failed")
                    }
                }
            }

            /// Close and reopen ALSA device with new format.
            /// Some hardware (e.g. XMOS USB controllers) can't reconfigure
            /// HW params in-place after snd_pcm_drop() — need full close+reopen.
            fn reopen_alsa(
                device: &str,
                fmt: &PcmFormat,
                sr: &AtomicU32,
                sbuf: &mut Vec<u8>,
                bit_perfect: bool,
                exact_rate: bool,
            ) -> Result<(alsa::PCM, PcmFormat), String> {
                let pcm = alsa::PCM::new(device, alsa::Direction::Playback, true)
                    .map_err(|e| format!("Failed to reopen ALSA device: {e}"))?;
                let negotiated = configure_alsa_hwparams(&pcm, fmt, bit_perfect, exact_rate)?;
                pcm.prepare().map_err(|e| format!("pcm.prepare: {e}"))?;
                sr.store(negotiated.sample_rate, Ordering::Relaxed);
                let silence_frames = (negotiated.sample_rate as usize * 50) / 1000;
                *sbuf = vec![0u8; silence_frames * negotiated.channels as usize * negotiated.bytes_per_sample as usize];
                Ok((pcm, negotiated))
            }

            log::info!(
                "[alsa-writer] started, device={device}, format={}, rate={}Hz, channels={}, bps={}, combined_vol={}",
                current_fmt.gst_format, current_fmt.sample_rate, current_fmt.channels, current_fmt.bytes_per_sample,
                f32::from_bits(combined_vol.load(Ordering::Relaxed))
            );

            let mut fir_slot = FirSlot::new();
            let mut fir_live = false;
            let mut last_data_generation: u64 = 0;

            'main: loop {
                if cancelled.load(Ordering::Acquire) { break; }
                match rx.recv_timeout(period_duration) {
                    Ok(WriterCommand::Data(mut chunk)) => {
                        if chunk.generation < writer_gen.load(Ordering::Acquire) {
                            continue; // discard stale data from old pipeline
                        }

                        // Pause: freeze output immediately, spin until resumed
                        if paused.load(Ordering::Acquire) {
                            let can_hw = pcm.state() == alsa::pcm::State::Running
                                && pcm.hw_params_current().map(|p| p.can_pause()).unwrap_or(false);
                            if can_hw { pcm.pause(true).ok(); }

                            while paused.load(Ordering::Acquire) {
                                if cancelled.load(Ordering::Acquire) { break 'main; }
                                if can_hw {
                                    // HW pause: DAC frozen, nothing to feed — just sleep
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                } else {
                                    // SW pause: blocking writei paces the thread (~50ms per period)
                                    if let Err(kind) = write_silence(&pcm, &silence_buf) {
                                if kind == "cancelled" { break 'main; }
                                        *decoded_cell.lock().unwrap() = None;
                                        *output_cell.lock().unwrap() = None;
                                        app_handle.emit("audio-error",
                                            serde_json::json!({ "kind": kind })).ok();
                                        tearing_down.store(true, Ordering::SeqCst);
                                        break 'main;
                                    }
                                }
                            }

                            if can_hw {
                                pcm.pause(false).ok();
                            } else {
                                // Clear silence from ring buffer after software pause
                                pcm.drop().ok();
                                pcm.prepare().ok();
                            }

                            // Re-check generation — may have changed during pause (track change)
                            if chunk.generation < writer_gen.load(Ordering::Acquire) {
                                continue;
                            }
                        }

                        if chunk.format != current_fmt {
                            log::info!("[alsa-writer] format change: {current_fmt:?} -> {:?}", chunk.format);
                            drop(pcm);
                            match reopen_alsa(&device, &chunk.format, &current_sample_rate, &mut silence_buf, bit_perfect, preserve_rate) {
                                Ok((new_pcm, negotiated)) => {
                                    pcm = new_pcm;
                                    if negotiated.gst_format != chunk.format.gst_format
                                        || negotiated.channels != chunk.format.channels
                                        || (bit_perfect && negotiated.sample_rate != chunk.format.sample_rate) {
                                        log::error!(
                                            "[alsa-writer] format mismatch after reopen: chunk={}/{}ch, ALSA={}/{}ch",
                                            chunk.format.gst_format, chunk.format.channels,
                                            negotiated.gst_format, negotiated.channels
                                        );
                                        app_handle.emit("audio-error",
                                            serde_json::json!({ "kind": "device_changed" })).ok();
                                        tearing_down.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                    sp.set_output(&negotiated.gst_format, negotiated.sample_rate, negotiated.channels);
                                    if !bit_perfect && chunk.format.gst_format != negotiated.gst_format {
                                        sp.record_format_fallback(&chunk.format.gst_format, &negotiated.gst_format);
                                    } else {
                                        sp.clear_format_fallback();
                                    }
                                    current_fmt = negotiated;
                                }
                                Err(e) => {
                                    log::error!("[alsa-writer] reopen failed: {e}");
                                    app_handle.emit("audio-error", serde_json::json!({ "kind": if bit_perfect { "bit_perfect_unsupported" } else { "format_change_failed" }, "message": e })).ok();
                                    tearing_down.store(true, Ordering::SeqCst);
                                    return; // pcm already dropped, just exit thread
                                }
                            }
                        }
                        resolve_pending(&mut pending_promotion_from, &current_fmt);
                        if chunk.generation != last_data_generation {
                            fir_slot.discard();
                            last_data_generation = chunk.generation;
                        }
                        let vol = f32::from_bits(combined_vol.load(Ordering::Relaxed));
                        if let Err(kind) = play_chunk(
                            &pcm,
                            &mut chunk.data,
                            &chunk.format,
                            &current_fmt,
                            vol,
                            &mut tpdf,
                            &mut fir_slot,
                            &mut fir_live,
                            sp.as_ref(),
                            &app_handle,
                            frames_written.as_ref(),
                            &silence_buf,
                            &cancelled,
                            &paused,
                        ) {
                            if kind == "cancelled" { break 'main; }
                            app_handle.emit("audio-error", serde_json::json!({ "kind": kind })).ok();
                            tearing_down.store(true, Ordering::SeqCst);
                            break;
                        }
                    }

                    Ok(WriterCommand::FormatHint(new_fmt)) => {
                        if new_fmt != current_fmt {
                            log::info!("[alsa-writer] format hint: {current_fmt:?} -> {new_fmt:?}");
                            let requested = new_fmt.clone();
                            drop(pcm);
                            match reopen_alsa(&device, &new_fmt, &current_sample_rate, &mut silence_buf, bit_perfect, preserve_rate) {
                                Ok((new_pcm, negotiated)) => {
                                    pcm = new_pcm;
                                    // Format fallback is allowed here (handled below); a
                                    // CHANNEL mismatch is not — it would misframe writes.
                                    if negotiated.channels != requested.channels {
                                        log::error!(
                                            "[alsa-writer] channel mismatch after format-hint reopen: requested={}ch, ALSA={}ch",
                                            requested.channels, negotiated.channels
                                        );
                                        app_handle.emit("audio-error",
                                            serde_json::json!({ "kind": "device_changed" })).ok();
                                        tearing_down.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                    sp.set_output(&negotiated.gst_format, negotiated.sample_rate, negotiated.channels);
                                    if !bit_perfect && requested.gst_format != negotiated.gst_format {
                                        sp.record_format_fallback(&requested.gst_format, &negotiated.gst_format);
                                    } else {
                                        sp.clear_format_fallback();
                                    }
                                    current_fmt = negotiated;
                                }
                                Err(e) => {
                                    log::error!("[alsa-writer] reopen for format hint failed: {e}");
                                    app_handle.emit("audio-error", serde_json::json!({ "kind": if bit_perfect { "bit_perfect_unsupported" } else { "format_change_failed" }, "message": e })).ok();
                                    tearing_down.store(true, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                    }

                    Ok(WriterCommand::PendingPromotion { from, generation }) => {
                        if generation < writer_gen.load(Ordering::Acquire) {
                            continue; // stale promotion from old pipeline
                        }
                        // Last-write-wins: overwrites any prior unresolved pending.
                        // resolve_pending will fire sp.record_bit_depth_promotion()
                        // once the actually-negotiated format is known.
                        pending_promotion_from = Some(from);
                    }

                    Ok(WriterCommand::EndOfTrack { emit_finished, generation }) => {
                        if generation < writer_gen.load(Ordering::Acquire) {
                            continue; // stale EOS from old pipeline
                        }
                        let vol = f32::from_bits(combined_vol.load(Ordering::Relaxed));
                        let mut end_failed = false;
                        match fir_slot.flush(
                            current_fmt.sample_rate,
                            current_fmt.channels,
                            &current_fmt.gst_format,
                            vol,
                            &mut || tpdf.triangular(),
                        ) {
                            FirOutput::Processed(bytes) => {
                                if !bytes.is_empty() {
                                    if let Err(kind) = write_bytes(
                                        &pcm,
                                        &bytes,
                                        &current_fmt,
                                        &frames_written,
                                        &silence_buf,
                                    ) {
                                        app_handle
                                            .emit("audio-error", serde_json::json!({ "kind": kind }))
                                            .ok();
                                        tearing_down.store(true, Ordering::SeqCst);
                                        end_failed = true;
                                    }
                                }
                            }
                            FirOutput::Failed(message) => {
                                report_fir_failure(&app_handle, &message);
                                app_handle.emit("audio-error", serde_json::json!({"kind":"dsp_processing_failed", "message":message})).ok();
                                tearing_down.store(true, Ordering::SeqCst);
                                end_failed = true;
                            },
                            FirOutput::Off => {}
                        }
                        if end_failed {
                            break 'main;
                        }
                        fir_slot.discard();
                        if let Err(kind) = write_silence(&pcm, &silence_buf) {
                                if kind == "cancelled" { break 'main; }
                            *decoded_cell.lock().unwrap() = None;
                            *output_cell.lock().unwrap() = None;
                            app_handle.emit("audio-error",
                                serde_json::json!({ "kind": kind })).ok();
                            tearing_down.store(true, Ordering::SeqCst);
                            break 'main;
                        }

                        if emit_finished && !tearing_down.load(Ordering::SeqCst) {
                            log::debug!("[alsa-writer] emitting track-finished");
                            app_handle.emit("track-finished", ()).ok();
                        }


                        // Idle silence loop — keep DAC clock alive between tracks
                        log::debug!("[alsa-writer] entering idle silence loop");
                        loop {
                            if cancelled.load(Ordering::Acquire) { break 'main; }
                            if let Err(kind) = write_silence(&pcm, &silence_buf) {
                                if kind == "cancelled" { break 'main; }
                                *decoded_cell.lock().unwrap() = None;
                                *output_cell.lock().unwrap() = None;
                                app_handle.emit("audio-error",
                                    serde_json::json!({ "kind": kind })).ok();
                                tearing_down.store(true, Ordering::SeqCst);
                                break 'main;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            match rx.try_recv() {
                                Ok(WriterCommand::Data(mut chunk)) => {
                                    if chunk.generation < writer_gen.load(Ordering::Acquire) {
                                        continue; // discard stale data, stay in idle
                                    }
                                    if chunk.format != current_fmt {
                                                    // reopen_alsa drops old PCM — buffer cleared implicitly
                                        drop(pcm);
                                        match reopen_alsa(&device, &chunk.format, &current_sample_rate, &mut silence_buf, bit_perfect, preserve_rate) {
                                            Ok((new_pcm, negotiated)) => {
                                                pcm = new_pcm;
                                                if negotiated.gst_format != chunk.format.gst_format
                                                    || negotiated.channels != chunk.format.channels {
                                                    log::error!(
                                                        "[alsa-writer] format mismatch after reopen (idle): chunk={}/{}ch, ALSA={}/{}ch",
                                                        chunk.format.gst_format, chunk.format.channels,
                                                        negotiated.gst_format, negotiated.channels
                                                    );
                                                    app_handle.emit("audio-error",
                                                        serde_json::json!({ "kind": "device_changed" })).ok();
                                                    tearing_down.store(true, Ordering::SeqCst);
                                                    return;
                                                }
                                                sp.set_output(&negotiated.gst_format, negotiated.sample_rate, negotiated.channels);
                                                if !bit_perfect && chunk.format.gst_format != negotiated.gst_format {
                                                    sp.record_format_fallback(&chunk.format.gst_format, &negotiated.gst_format);
                                                } else {
                                                    sp.clear_format_fallback();
                                                }
                                                current_fmt = negotiated;
                                            }
                                            Err(e) => {
                                                log::error!("[alsa-writer] reopen failed in idle: {e}");
                                                app_handle.emit("audio-error", serde_json::json!({ "kind": if bit_perfect { "bit_perfect_unsupported" } else { "format_change_failed" }, "message": e })).ok();
                                                return;
                                            }
                                        }
                                    } else {
                                        // Same format — flush stale silence from ring buffer
                                        pcm.drop().ok();
                                        pcm.prepare().ok();
                                    }
                                    resolve_pending(&mut pending_promotion_from, &current_fmt);
                                    if chunk.generation != last_data_generation {
                                        fir_slot.discard();
                                        last_data_generation = chunk.generation;
                                    }
                                    let vol = f32::from_bits(combined_vol.load(Ordering::Relaxed));
                                    if let Err(kind) = play_chunk(
                                        &pcm,
                                        &mut chunk.data,
                                        &chunk.format,
                                        &current_fmt,
                                        vol,
                                        &mut tpdf,
                                        &mut fir_slot,
                                        &mut fir_live,
                                        sp.as_ref(),
                                        &app_handle,
                                        frames_written.as_ref(),
                                        &silence_buf,
                                        &cancelled,
                                        &paused,
                                    ) {
                                        if kind == "cancelled" { break 'main; }
                                        app_handle.emit("audio-error", serde_json::json!({ "kind": kind })).ok();
                                        break 'main;
                                    }
                                    break; // back to main loop
                                }
                                Ok(WriterCommand::Shutdown) => break 'main,
                                Ok(WriterCommand::Flush) => {
                                    pcm.drop().ok();
                                    pcm.prepare().ok();
                                    pending_promotion_from = None;
                                    fir_slot.discard();
                                    break;
                                }
                                Ok(WriterCommand::SetFir { path, generation }) => {
                                    if fir_slot.set_path(path, generation) && fir_live {
                                        fir_live = false;
                                        sp.set_camilla_fir(false);
                                    }
                                }
                                Ok(WriterCommand::FormatHint(new_fmt)) => {
                                                if new_fmt != current_fmt {
                                        log::info!("[alsa-writer] format hint (idle): {current_fmt:?} -> {new_fmt:?}");
                                        let requested = new_fmt.clone();
                                        drop(pcm);
                                        match reopen_alsa(&device, &new_fmt, &current_sample_rate, &mut silence_buf, bit_perfect, preserve_rate) {
                                            Ok((new_pcm, negotiated)) => {
                                                pcm = new_pcm;
                                                if negotiated.channels != requested.channels {
                                                    log::error!(
                                                        "[alsa-writer] channel mismatch after format-hint reopen (idle): requested={}ch, ALSA={}ch",
                                                        requested.channels, negotiated.channels
                                                    );
                                                    app_handle.emit("audio-error",
                                                        serde_json::json!({ "kind": "device_changed" })).ok();
                                                    tearing_down.store(true, Ordering::SeqCst);
                                                    return;
                                                }
                                                sp.set_output(&negotiated.gst_format, negotiated.sample_rate, negotiated.channels);
                                                if !bit_perfect && requested.gst_format != negotiated.gst_format {
                                                    sp.record_format_fallback(&requested.gst_format, &negotiated.gst_format);
                                                } else {
                                                    sp.clear_format_fallback();
                                                }
                                                current_fmt = negotiated;
                                            }
                                            Err(e) => {
                                                log::error!("[alsa-writer] reopen for format hint failed (idle): {e}");
                                                app_handle.emit("audio-error", serde_json::json!({ "kind": if bit_perfect { "bit_perfect_unsupported" } else { "format_change_failed" }, "message": e })).ok();
                                                return;
                                            }
                                        }
                                    }
                                }
                                Ok(WriterCommand::Resampling { from, to }) => {
                                    sp.record_resample(from, to);
                                }
                                Ok(WriterCommand::PendingPromotion { from, generation }) => {
                                    if generation < writer_gen.load(Ordering::Acquire) {
                                        continue;
                                    }
                                    pending_promotion_from = Some(from);
                                }
                                Ok(_) => {}
                                Err(crossbeam_channel::TryRecvError::Empty) => {}
                                Err(crossbeam_channel::TryRecvError::Disconnected) => break 'main,
                            }
                        }
                    }

                    Ok(WriterCommand::Flush) => {
                        pcm.drop().ok();
                        pcm.prepare().ok();
                        pending_promotion_from = None;
                        fir_slot.discard();
                    }

                    Ok(WriterCommand::Resampling { from, to }) => {
                        sp.record_resample(from, to);
                        app_handle.emit("audio-resampled", serde_json::json!({ "from": from, "to": to })).ok();
                    }

                    Ok(WriterCommand::SetFir { path, generation }) => {
                        if fir_slot.set_path(path, generation) && fir_live {
                            fir_live = false;
                            sp.set_camilla_fir(false);
                        }
                    }

                    Ok(WriterCommand::Shutdown) => {
                        log::debug!("[alsa-writer] shutdown");
                        pcm.drop().ok();
                        break;
                    }

                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if let Err(kind) = write_silence(&pcm, &silence_buf) {
                                if kind == "cancelled" { break 'main; }
                            *decoded_cell.lock().unwrap() = None;
                            *output_cell.lock().unwrap() = None;
                            app_handle.emit("audio-error",
                                serde_json::json!({ "kind": kind })).ok();
                            tearing_down.store(true, Ordering::SeqCst);
                            break 'main;
                        }
                    }

                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        log::debug!("[alsa-writer] channel disconnected");
                        pcm.drop().ok();
                        break;
                    }
                }
            }

            log::info!("[alsa-writer] thread exiting");
        })
        .map_err(|e| format!("Failed to spawn ALSA writer thread: {e}"))?;

    Ok((
        tx,
        handle,
        negotiated_fmt,
        supported_gst_formats,
        supported_rates,
    ))
}

// ── Audio command protocol ─────────────────────────────────────────────

enum AudioCommand {
    PlayUrl {
        uri: String,
        /// Where the new pipeline should start. `None` is a normal track start;
        /// `Some` is a rebuild that has to resume where the torn-down pipeline
        /// was, which is how a mid-track route change stays inaudible.
        start_secs: Option<f32>,
        preserve_output: bool,
        track_id: Option<u64>,
        resume_paused: bool,
        reply: Reply<Result<(), String>>,
    },
    Pause {
        reply: Reply<Result<(), String>>,
    },
    Resume {
        reply: Reply<Result<(), String>>,
    },
    Stop {
        reply: Reply<Result<(), String>>,
    },
    SetVolume {
        level: f32,
        reply: Reply<Result<(), String>>,
    },
    SetNormalizationGain {
        gain: f64,
        reply: Reply<Result<(), String>>,
    },
    Seek {
        position_secs: f32,
        reply: Reply<Result<(), String>>,
    },
    GetPosition {
        reply: Reply<Result<f32, String>>,
    },
    IsFinished {
        reply: Reply<Result<bool, String>>,
    },
    OutputChanged,
    SetProxySettings {
        settings: crate::ProxySettings,
        reply: Reply<()>,
    },
    SetGapless {
        enabled: bool,
        reply: Reply<Result<(), String>>,
    },
    // 2b-A2: fields drive the preroll attach (uri, gating metadata, qid).
    SetNextTrack {
        uri: String,
        norm_gain: f64,
        track_id: u64,
        qid: String,
        replay_gain: f64,
        peak_amplitude: f64,
        is_dash: bool,
        reply: Reply<Result<(), String>>,
    },
    ClearNextTrack {
        reply: Reply<Result<(), String>>,
    },
    /// 2b-A3: emitted by concat's notify::active-pad handler once it has verified
    /// (by pad identity) that concat switched to the prerolled next branch.
    /// Fieldless (C6) — the handler reads everything from the `next_bin` slot.
    HandleGaplessAdvance,
    /// HQPlayer stayed in play and its position restarted near the start of the
    /// queued WAV. Fieldless: the handler takes the prepared next slot.
    HandleHqAdvance,
    /// 2b-A3: forwarded by the Normal bus watcher when an Error originates inside
    /// the prerolled next bin. The worker detaches that bin (gated on
    /// !next_active) without disturbing the currently-playing track.
    HandleNextBinError,
    ListDevices {
        reply: Reply<Result<Vec<AudioDevice>, String>>,
    },
}

// ── AudioPlayer (public API unchanged) ─────────────────────────────────

#[derive(Clone)]
pub struct AudioPlayer {
    cmd_tx: mpsc::Sender<AudioCommand>,
    output_state: Arc<Mutex<AudioOutputState>>,
    output_transaction: Arc<Mutex<()>>,
    /// Latest exclusive ALSA device set via `SetExclusiveMode`. Mirrored from
    /// the audio thread so the pipeline probe can read it without messaging.
    exclusive_device: Arc<Mutex<Option<String>>>,
    decoded_caps_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    output_caps_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
}

impl AudioPlayer {
    pub fn new(
        app_handle: tauri::AppHandle,
        signal_path: Arc<SignalPathTracker>,
        proxy_settings: crate::ProxySettings,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        // Clone a self-sender into the worker so the Normal bus thread can send
        // HandleGaplessAdvance back to this loop (Task 3). `cmd_tx` itself is
        // owned by AudioPlayer, not the worker closure.
        let cmd_tx_worker = cmd_tx.clone();
        let output_state = Arc::new(Mutex::new(AudioOutputState::default()));
        let output_state_thread = Arc::clone(&output_state);
        let output_transaction = Arc::new(Mutex::new(()));
        let output_transaction_thread = Arc::clone(&output_transaction);
        let exclusive_device: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let exclusive_device_thread = exclusive_device.clone();
        let decoded_caps_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>> =
            Arc::new(Mutex::new(None));
        let output_caps_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>> =
            Arc::new(Mutex::new(None));
        let decoded_cell_thread = Arc::clone(&decoded_caps_cell);
        let output_cell_thread = Arc::clone(&output_caps_cell);

        std::thread::spawn(move || {
            // GST_PLUGIN_PATH is set in main(), before any thread exists.
            gst::init().expect("Failed to initialize GStreamer");

            // Captured once, before anything promotes it. Re-reading this at a
            // later call site would bake the promoted rank in as "original" and
            // the rank would never come back down.
            let original_curl_rank =
                gst::ElementFactory::find("curlhttpsrc").map(|factory| factory.rank());
            let probed = probe_host_caps();

            // Seed from the settings the constructor already received. Without
            // this, nothing pushes a route until the user next presses Save, and
            // a launch with a saved proxy would play every track direct. Moved in
            // by value: `audio_proxy` below is the one live copy the worker reads,
            // and `SetProxySettings` replaces it, so a second shared cell would
            // only be written and never read.
            let audio_proxy = Arc::new(Mutex::new(AudioProxy::new(proxy_settings, probed)));

            // Promotion follows whichever tier is routable; when both are, the
            // routes are identical, so either answers the credentials question.
            {
                let ap = audio_proxy
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let probe_route = ap
                    .route_for(crate::proxy::Capability::Lossy)
                    .or_else(|_| ap.route_for(crate::proxy::Capability::Dash))
                    .unwrap_or(crate::proxy::Route::NoProxy);
                promote_curl_source(&probe_route, original_curl_rank);
            }

            let mut backend: Option<PlaybackBackend> = None;
            // ALSA writer state — lives outside PlaybackBackend so it persists across track changes
            let mut writer_tx: Option<crossbeam_channel::Sender<WriterCommand>> = None;
            let mut writer_thread: Option<JoinHandle<()>> = None;
            let mut writer_cancel = Arc::new(AtomicBool::new(false));
            let mut writer_fmt: Option<PcmFormat> = None;
            let mut writer_supported_fmts: Option<Vec<&'static str>> = None;
            let mut writer_supported_rates: Option<Vec<u32>> = None;
            let mut writer_device: Option<String> = None;
            // Track the mode the live writer was spawned in. `bit_perfect` is
            // baked into the writer thread at spawn (it drives reopen format
            // negotiation), so a same-device exclusive↔bit-perfect toggle must
            // force a respawn rather than reuse a stale-mode writer.
            let mut writer_bit_perfect: Option<bool> = None;
            let mut writer_preserve_rate = false;
            let frames_written = Arc::new(AtomicU64::new(0));
            let current_sample_rate = Arc::new(AtomicU32::new(48000));
            let writer_gen = Arc::new(AtomicU64::new(0));
            let paused = Arc::new(AtomicBool::new(false));
            let combined_vol = Arc::new(AtomicU32::new(1.0_f32.to_bits()));

            let eos = Arc::new(AtomicBool::new(false));
            let tearing_down = Arc::new(AtomicBool::new(false));
            let has_uri = AtomicBool::new(false);

            let mut exclusive = false;
            let mut bit_perfect = false;
            let mut device: Option<String> = None;
            // CamillaDSP FIR path. None leaves exclusive output as direct PCM.
            // Applied to the ALSA writer on the next PlayUrl, not mid-buffer.
            #[cfg(target_os = "linux")]
            let mut camilla_path: Option<String> = None;
            #[cfg(target_os = "linux")]
            let mut camilla_generation: u64 = 0;

            let mut current_volume: f64 = 1.0;
            let mut current_norm_gain: f64 = 1.0;
            let mut track_generation: u64 = 0;
            // 2b: under `concat` (adjust-base=true), `query_position(TIME)` re-bases
            // to ~0 at each track boundary — it is per-track, NOT cumulative. So the
            // 2a `position_offset_ns` capture/subtract mechanism is gone (C2).

            // Gapless state. `gapless_setting` defaults to true and is pushed
            // from saved settings at startup via `set_gapless` (the worker has
            // no access to AppState). `cmd_tx_worker` is the self-sender the
            // Normal bus thread clones for gapless advance handling (2b-A3).
            let mut gapless_setting: bool = true;
            let cmd_tx_worker = cmd_tx_worker;
            // HQPlayer Desktop handoff. The flag is shared with the gapless
            // executor so a preroll already in flight is not armed after the
            // switch. The committed configuration selects a loopback address
            // and control port; pending settings never change this live endpoint.
            let hq_enabled = Arc::new(AtomicBool::new(false));
            let mut hq_host = "127.0.0.1".to_string();
            let mut hq_port: u16 = crate::hqplayer::DEFAULT_PORT;
            let hq_control: Arc<Mutex<Option<crate::hqplayer::ControlSession>>> =
                Arc::new(Mutex::new(None));
            let hq_gen = Arc::new(AtomicU64::new(0));
            // `started_gen` is the `hq_gen` whose watcher has seen state 2.
            // A bool would let the previous watcher mark the next track started.
            let hq_watch = Arc::new(HqWatch {
                cmd_tx: cmd_tx_worker.clone(),
                next: Mutex::new(None),
                preparing: Mutex::new(None),
                last_status: Mutex::new(None),
                output_state: Arc::clone(&output_state_thread),
                output_transaction: Arc::clone(&output_transaction_thread),
                started_gen: AtomicU64::new(0),
                advancing: AtomicBool::new(false),
                next_gen: AtomicU64::new(0),
            });

            // 2b-A2: the prerolled next bin. Shared between this worker (dedup /
            // replace / gating), the attach executor (which fills it), and the
            // notify::active-pad handler (2b-A3 advance). Locking is brief and
            // never held across a blocking GStreamer call: the worker locks only
            // to read `track_id`/`qid` for dedup or to overwrite the slot on
            // dispatch; the executor locks only to store/clear after the
            // (off-thread) attach completes. No lock is held across `pipeline.add`
            // / `sync_state_with_parent` / `set_state(Null)` → no deadlock path.
            let next_bin: Arc<Mutex<Option<NextBinState>>> = Arc::new(Mutex::new(None));
            // Set true by 2b-A3's notify handler while concat is switching to the
            // next bin; gates detach/replace (C5) so we never tear down a bin
            // mid-advance. Read by 2b-A3.
            let next_active = Arc::new(AtomicBool::new(false));
            // 2b-A3: the (uridecodebin, branch_queue) of the CURRENTLY-PLAYING
            // track's branch (concat sink_0 at build time). On a gapless advance
            // this finished branch is detached and replaced by the promoted next
            // branch. None when no Normal pipeline is live (or DirectAlsa).
            let mut current_branch: Option<(gst::Element, gst::Element)> = None;
            // The URI of whatever is playing now. The worker otherwise retains
            // none: `uri` is local to the `PlayUrl` arm, and the only surviving
            // state is `has_uri`. Without it a route change mid-track has nothing
            // to re-issue, and this whole path is a no-op.
            let mut current_uri: Option<String> = None;
            let mut current_track_id: Option<u64> = None;
            // Bumped by every `SetProxySettings`. The executor snapshots it before
            // it starts prerolling and re-reads it under the `next_bin` mutex
            // before storing, so a branch built under the previous route is
            // discarded instead of being armed as the next playing track.
            let route_generation = Arc::new(AtomicU64::new(0));
            // The generation the live pipeline's hook was built under. Sampled
            // at each build in `PlayUrl`, carried in every `AttachJob::Attach`,
            // and compared there: `SetProxySettings` bumps the generation and
            // only then queues the rebuild on the self-sender, so a
            // `SetNextTrack` landing in that gap is served by the pipeline that
            // is about to be replaced.
            let mut pipeline_route_generation: u64 = 0;

            // Serialized attach/detach executor thread (C3). The worker dispatches
            // jobs here and returns immediately — it never blocks on pad-slot ops.
            let (attach_tx, attach_rx) = mpsc::channel::<AttachJob>();
            {
                let next_bin_exec = Arc::clone(&next_bin);
                let audio_proxy_exec = Arc::clone(&audio_proxy);
                let route_generation_exec = Arc::clone(&route_generation);
                let hq_enabled_exec = Arc::clone(&hq_enabled);
                std::thread::spawn(move || {
                    run_attach_executor(
                        attach_rx,
                        next_bin_exec,
                        audio_proxy_exec,
                        route_generation_exec,
                        hq_enabled_exec,
                    )
                });
            }

            for cmd in cmd_rx {
                match cmd {
                    AudioCommand::PlayUrl {
                        uri,
                        start_secs,
                        preserve_output,
                        track_id,
                        resume_paused,
                        reply,
                    } => {
                        let mut config = {
                            let _transaction = output_transaction_thread.lock().unwrap();
                            let state = output_state_thread.lock().unwrap();
                            playback_config(&state, preserve_output)
                        };
                        let mut startup = None;
                        let result = (|| -> Result<(), String> {
                            config.validate()?;
                            if matches!(backend.as_ref(), Some(PlaybackBackend::HqPlayer { .. })) {
                                hq_send_stop(&hq_control, &hq_host, hq_port)?;
                            }
                            // A queued HQPlayer WAV belongs to the track being
                            // replaced. Drop it before the new handoff clears
                            // Desktop's playlist.
                            cancel_hq_prepared(&hq_watch, &hq_control, &hq_host, hq_port);
                            hq_watch.started_gen.store(0, Ordering::Release);
                            *hq_watch.last_status.lock().unwrap() = None;
                            hq_watch.advancing.store(false, Ordering::Release);
                            // ── Teardown old backend (GStreamer pipeline only) ──
                            if let Some(old_backend) = backend.take() {
                                tearing_down.store(true, Ordering::SeqCst);

                                match old_backend {
                                    PlaybackBackend::Normal {
                                        pipeline,
                                        user_volume_el,
                                        ..
                                    } => {
                                        if let Some(bus) = pipeline.bus() {
                                            bus.set_flushing(true);
                                        }
                                        let old_pipe = pipeline;
                                        std::thread::spawn(move || {
                                            // Fade out
                                            if let Some(ref vol) = user_volume_el {
                                                for i in (0..=10).rev() {
                                                    vol.set_property(
                                                        "volume",
                                                        slider_to_amplitude(current_volume)
                                                            * (i as f64 / 10.0),
                                                    );
                                                    std::thread::sleep(
                                                        std::time::Duration::from_millis(10),
                                                    );
                                                }
                                            }
                                            old_pipe.set_state(gst::State::Null).ok();
                                        });
                                    }
                                    PlaybackBackend::DirectAlsa { pipeline, .. } => {
                                        // Unblock writer if paused, then bump generation —
                                        // writer instantly discards stale Data, channel
                                        // drains fast, pipeline can reach Null without blocking.
                                        paused.store(false, Ordering::Release);
                                        track_generation += 1;
                                        writer_gen.store(track_generation, Ordering::Release);
                                        if let Some(ref tx) = writer_tx {
                                            let _ = tx.send_timeout(
                                                WriterCommand::Flush,
                                                std::time::Duration::from_millis(200),
                                            );
                                        }
                                        if let Some(bus) = pipeline.bus() {
                                            bus.set_flushing(true);
                                        }
                                        pipeline.set_state(gst::State::Null).ok();
                                        let _ = pipeline.state(gst::ClockTime::from_mseconds(500));
                                        drop(pipeline);
                                    }
                                    PlaybackBackend::HqPlayer {
                                        pipeline,
                                        control,
                                        feed,
                                        finish_emit,
                                        ..
                                    } => {
                                        // Stop the watcher from treating this as the end
                                        // of the track, then tell Desktop to release the DAC.
                                        finish_emit.store(false, Ordering::SeqCst);
                                        feed.cancel();
                                        bump_hq_generation(&output_transaction_thread, &hq_gen);
                                        let _ = control;
                                        if let Some(bus) = pipeline.bus() {
                                            bus.set_flushing(true);
                                        }
                                        pipeline.set_state(gst::State::Null).ok();
                                        let _ = pipeline.state(gst::ClockTime::from_mseconds(500));
                                        drop(pipeline);
                                    }
                                }

                                log::debug!("[audio] teardown: complete");
                            }
                            publish_output_state(
                                &app_handle,
                                &output_state_thread,
                                &output_transaction_thread,
                                None,
                            );
                            // 2b-A3 (detach matrix): the whole old pipeline is being
                            // torn down (its elements die with it), so we don't dispatch
                            // an executor detach — just null the gapless slots. The
                            // executor detach would be moot (and could race the new
                            // pipeline). Clearing `next_active` releases any in-flight
                            // advance gate, which is correct on a hard re-play.
                            if let Ok(mut g) = next_bin.lock() {
                                *g = None;
                            }
                            next_active.store(false, Ordering::Release);
                            current_branch = None;

                            tearing_down.store(false, Ordering::SeqCst);
                            eos.store(false, Ordering::SeqCst);
                            has_uri.store(true, Ordering::SeqCst);
                            frames_written.store(0, Ordering::Relaxed);

                            *decoded_cell_thread.lock().unwrap() = None;
                            *output_cell_thread.lock().unwrap() = None;
                            signal_path.reset_for_track();
                            exclusive = config.exclusive_mode;
                            bit_perfect = config.bit_perfect;
                            device = config.device.clone();
                            camilla_path = if config.route == AudioOutputRoute::Camilla {
                                config.camilla_config.clone()
                            } else {
                                None
                            };
                            camilla_generation = camilla_generation.wrapping_add(1);
                            paused.store(resume_paused, Ordering::Release);
                            hq_enabled.store(
                                config.route == AudioOutputRoute::Hqplayer,
                                Ordering::Release,
                            );
                            hq_host = config.hqplayer_host.clone();
                            hq_port = config.hqplayer_port;
                            *exclusive_device_thread.lock().unwrap() =
                                if exclusive { device.clone() } else { None };
                            signal_path.set_camilla_fir(camilla_path.is_some());
                            let amplitude = slider_to_amplitude(current_volume);
                            combined_vol.store(
                                (if bit_perfect {
                                    1.0f32
                                } else {
                                    (amplitude * current_norm_gain) as f32
                                })
                                .to_bits(),
                                Ordering::Relaxed,
                            );
                            signal_path.set_user_volume(if bit_perfect {
                                1.0
                            } else {
                                amplitude as f32
                            });
                            signal_path.set_norm_gain_factor(if bit_perfect {
                                1.0
                            } else {
                                current_norm_gain as f32
                            });
                            let hand_to_hqplayer = hq_enabled.load(Ordering::Acquire);
                            if hand_to_hqplayer {
                                signal_path.set_backend("HQPlayer", None);
                                signal_path.set_audio_modes(false, false);
                                signal_path.set_user_volume(1.0);
                                signal_path.set_norm_gain_factor(1.0);
                            } else if exclusive || bit_perfect {
                                signal_path.set_backend("DirectAlsa", device.clone());
                                signal_path.set_audio_modes(exclusive, bit_perfect);
                            } else {
                                signal_path.set_backend("Normal", None);
                                signal_path.set_audio_modes(exclusive, bit_perfect);
                            }

                            // One decision for both arms below. Each used to
                            // sniff the URI for itself, and neither could see
                            // the other's answer.
                            let is_dash = uri.starts_with("data:application/dash");
                            let route = {
                                let ap = audio_proxy.lock().unwrap_or_else(|p| p.into_inner());
                                ap.route_for(capability_of(is_dash))
                            };
                            let route = match route {
                                Ok(r) => r,
                                Err(blocked) => {
                                    // Refuse rather than play direct. This is the
                                    // containment boundary: the gapless advance
                                    // path reaches no Tauri command, so a
                                    // command-layer check cannot cover it.
                                    log::warn!("[proxy] refusing playback: {}", blocked.cause);
                                    return Err(format!("proxy blocked: {}", blocked.cause));
                                }
                            };
                            if matches!(route, crate::proxy::Route::Via { .. })
                                && !http_source_is_configurable()
                            {
                                return Err("proxy blocked: this system's https source \
                                            cannot be pointed at a proxy"
                                    .to_string());
                            }
                            // Stamp the pipeline both arms below are about to
                            // build with the generation this route came from.
                            pipeline_route_generation = route_generation.load(Ordering::Acquire);

                            if hand_to_hqplayer {
                                // DirectAlsa teardown leaves the writer holding the
                                // DAC. Desktop needs that device, so close it even
                                // when the control port is down.
                                writer_cancel.store(true, Ordering::Release);
                                release_alsa_writer(
                                    &mut writer_tx,
                                    &mut writer_thread,
                                    &mut writer_fmt,
                                    &mut writer_supported_fmts,
                                    &mut writer_supported_rates,
                                    &mut writer_device,
                                    &mut writer_bit_perfect,
                                );
                                let host = hq_host.clone();
                                let port = hq_port;
                                {
                                    let mut slot = hq_control
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    crate::hqplayer::retry_read(&mut slot, &host, port, |session| {
                                        session.get_info()
                                    })
                                    .map_err(|err| {
                                        format!(
                                            "HQPlayer is not accepting control on {host}:{port}. \
                                             Start HQPlayer Desktop and close its settings dialog ({err})"
                                        )
                                    })?;
                                }
                                let gen = bump_hq_generation(&output_transaction_thread, &hq_gen);
                                hq_watch.started_gen.store(0, Ordering::Release);
                                let origin = start_secs.unwrap_or(0.0);
                                let launch = launch_hqplayer_track(
                                    &uri,
                                    is_dash,
                                    route,
                                    origin,
                                    gen,
                                    Arc::clone(&hq_gen),
                                    Arc::clone(&hq_control),
                                    host,
                                    port,
                                    app_handle.clone(),
                                    Arc::clone(&tearing_down),
                                    Arc::clone(&eos),
                                    Arc::clone(&signal_path),
                                    Arc::clone(&decoded_cell_thread),
                                    Arc::clone(&output_cell_thread),
                                    Arc::clone(&hq_watch),
                                    None,
                                    resume_paused,
                                    track_id,
                                    Arc::clone(&paused),
                                )?;
                                startup = Some((
                                    launch.started,
                                    launch.playback_generation,
                                    launch.feed.clone(),
                                    launch.pipeline.clone(),
                                ));
                                backend = Some(PlaybackBackend::HqPlayer {
                                    pipeline: launch.pipeline,
                                    control: Arc::clone(&hq_control),
                                    feed: launch.feed,
                                    heard: launch.heard,
                                    finish_emit: launch.finish_emit,
                                });
                            } else if exclusive || bit_perfect {
                                // ── DirectAlsa path ──
                                #[cfg(not(target_os = "linux"))]
                                return Err("Exclusive/bit-perfect mode requires Linux".into());

                                #[cfg(target_os = "linux")]
                                {
                                    let dev = device.as_deref().ok_or_else(|| {
                                        "No audio device selected for exclusive mode".to_string()
                                    })?;

                                    let default_fmt = PcmFormat {
                                        sample_rate: 48000,
                                        channels: 2,
                                        gst_format: "S32LE".to_string(),
                                        bytes_per_sample: 4,
                                    };

                                    // Bump generation again: the old appsink may have
                                    // pushed chunks (stamped gen N+1) from its internal
                                    // queue between the Flush and set_state(Null).
                                    // Gen N+2 causes the writer to discard them instantly
                                    // instead of writing each to ALSA at audio rate (~85ms).
                                    track_generation += 1;
                                    writer_gen.store(track_generation, Ordering::Release);

                                    // Reuse writer if alive, otherwise spawn new one
                                    let writer_alive = writer_thread
                                        .as_ref()
                                        .map(|h| !h.is_finished())
                                        .unwrap_or(false);

                                    let device_changed = writer_device.as_deref() != Some(dev);
                                    let mode_changed = writer_bit_perfect != Some(bit_perfect)
                                        || writer_preserve_rate != camilla_path.is_some();

                                    if !writer_alive
                                        || writer_tx.is_none()
                                        || device_changed
                                        || mode_changed
                                    {
                                        // Shut down old writer cleanly
                                        writer_cancel.store(true, Ordering::Release);
                                        if let Some(tx) = writer_tx.take() {
                                            tx.try_send(WriterCommand::Shutdown).ok();
                                        }
                                        if let Some(h) = writer_thread.take() {
                                            h.join().ok();
                                        }
                                        writer_cancel = Arc::new(AtomicBool::new(false));
                                        let (
                                            tx,
                                            handle,
                                            negotiated_fmt,
                                            supported_gst_fmts,
                                            supported_rates,
                                        ) = spawn_alsa_writer(AlsaWriterConfig {
                                            device: dev,
                                            initial_format: &default_fmt,
                                            app_handle: app_handle.clone(),
                                            tearing_down: Arc::clone(&tearing_down),
                                            cancelled: Arc::clone(&writer_cancel),
                                            frames_written: Arc::clone(&frames_written),
                                            current_sample_rate: Arc::clone(&current_sample_rate),
                                            writer_gen: Arc::clone(&writer_gen),
                                            paused: Arc::clone(&paused),
                                            bit_perfect,
                                            preserve_rate: camilla_path.is_some(),
                                            combined_vol: Arc::clone(&combined_vol),
                                            signal_path: Arc::clone(&signal_path),
                                            decoded_cell: Arc::clone(&decoded_cell_thread),
                                            output_cell: Arc::clone(&output_cell_thread),
                                        })?;
                                        writer_tx = Some(tx);
                                        writer_thread = Some(handle);
                                        writer_fmt = Some(negotiated_fmt);
                                        writer_supported_fmts = Some(supported_gst_fmts);
                                        writer_supported_rates = Some(supported_rates);
                                        writer_device = Some(dev.to_string());
                                        writer_bit_perfect = Some(bit_perfect);
                                        writer_preserve_rate = camilla_path.is_some();
                                    }

                                    let wtx = writer_tx.as_ref().unwrap().clone();

                                    if let Err(err) = wtx.send_timeout(
                                        WriterCommand::SetFir {
                                            path: camilla_path.clone(),
                                            generation: camilla_generation,
                                        },
                                        std::time::Duration::from_millis(500),
                                    ) {
                                        return Err(format!("Could not arm CamillaDSP: {err}"));
                                    }

                                    // Build appsink pipeline
                                    let fmt_for_pipeline =
                                        writer_fmt.as_ref().unwrap_or(&default_fmt);
                                    let supported_fmts_for_pipeline =
                                        writer_supported_fmts.as_deref().unwrap_or(&["S32LE"]);
                                    let supported_rates_for_pipeline = writer_supported_rates
                                        .as_deref()
                                        .unwrap_or(&[44100, 48000]);
                                    let rejected = Arc::new(AtomicBool::new(false));
                                    let (pipe, u_vol, n_vol) =
                                        build_appsink_pipeline(AppSinkConfig {
                                            uri: &uri,
                                            is_dash,
                                            route,
                                            exclusive,
                                            bit_perfect,
                                            preserve_rate: camilla_path.is_some(),
                                            writer_tx: wtx.clone(),
                                            writer_gen: Arc::clone(&writer_gen),
                                            negotiated_fmt: fmt_for_pipeline,
                                            supported_gst_formats: supported_fmts_for_pipeline,
                                            supported_rates: supported_rates_for_pipeline,
                                            decoded_cell: Arc::clone(&decoded_cell_thread),
                                            output_cell: Arc::clone(&output_cell_thread),
                                            rejected: Arc::clone(&rejected),
                                            cancelled: Arc::clone(&writer_cancel),
                                        })?;

                                    // Start pipeline directly — errors come via bus watcher
                                    if let Err(error) = pipe.set_state(gst::State::Playing) {
                                        writer_cancel.store(true, Ordering::Release);
                                        let message = if rejected.load(Ordering::Acquire) {
                                            "bit_perfect_unsupported: DAC cannot preserve the source samples. Turn off bit-perfect manually to allow conversion.".to_string()
                                        } else {
                                            format!("Failed to start playback: {error}")
                                        };
                                        if rejected.load(Ordering::Acquire) {
                                            app_handle.emit("audio-error", serde_json::json!({ "kind": "bit_perfect_unsupported", "message": message })).ok();
                                        }
                                        pipe.set_state(gst::State::Null).ok();
                                        return Err(message);
                                    }

                                    // Bus watcher: decode errors + EOS → forward to writer
                                    let eos_flag = Arc::clone(&eos);
                                    let app_handle_clone = app_handle.clone();
                                    let writer_tx_bus = wtx;
                                    let bus_gen = Arc::clone(&writer_gen);
                                    let tearing_down_bus = Arc::clone(&tearing_down);
                                    let stop_tx = cmd_tx_worker.clone();
                                    if let Some(bus) = pipe.bus() {
                                        std::thread::spawn(move || {
                                            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                                                match msg.view() {
                                                    gst::MessageView::Eos(..) => {
                                                        if rejected.load(Ordering::Acquire) {
                                                            break;
                                                        }
                                                        eos_flag.store(true, Ordering::SeqCst);
                                                        writer_tx_bus
                                                            .send(WriterCommand::EndOfTrack {
                                                                emit_finished: true,
                                                                generation: bus_gen
                                                                    .load(Ordering::Acquire),
                                                            })
                                                            .ok();
                                                        break;
                                                    }
                                                    gst::MessageView::Error(err) => {
                                                        let err_msg = err.error().to_string();
                                                        let debug_str = err
                                                            .debug()
                                                            .map(|s| s.to_string())
                                                            .unwrap_or_default();
                                                        log::error!(
                                                            "GStreamer error: {} (debug: {})",
                                                            err_msg,
                                                            debug_str
                                                        );
                                                        eos_flag.store(true, Ordering::SeqCst);
                                                        if !tearing_down_bus.load(Ordering::SeqCst)
                                                        {
                                                            app_handle_clone
                                                                .emit(
                                                                    "audio-error",
                                                                    serde_json::json!({
                                                                        "kind": if rejected.load(Ordering::Acquire) { "bit_perfect_unsupported" } else { "playback_error" },
                                                                        "message": err_msg
                                                                    }),
                                                                )
                                                                .ok();
                                                        }
                                                        if rejected.load(Ordering::Acquire) {
                                                            let (reply, _) = mpsc::channel();
                                                            let _ = stop_tx
                                                                .send(AudioCommand::Stop { reply });
                                                            break;
                                                        }
                                                        writer_tx_bus
                                                            .send(WriterCommand::EndOfTrack {
                                                                emit_finished: false,
                                                                generation: bus_gen
                                                                    .load(Ordering::Acquire),
                                                            })
                                                            .ok();
                                                        break;
                                                    }
                                                    gst::MessageView::Buffering(b) => {
                                                        log::debug!(
                                                            "[audio] direct-alsa: buffering {}%",
                                                            b.percent()
                                                        );
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        });
                                    }

                                    backend = Some(PlaybackBackend::DirectAlsa {
                                        pipeline: pipe,
                                        user_volume_el: u_vol,
                                        norm_volume_el: n_vol,
                                    });
                                }
                            } else {
                                // ── Normal path (unchanged) ──
                                // Shut down any lingering ALSA writer from a mode switch
                                writer_cancel.store(true, Ordering::Release);
                                if let Some(tx) = writer_tx.take() {
                                    tx.try_send(WriterCommand::Shutdown).ok();
                                }
                                if let Some(h) = writer_thread.take() {
                                    h.join().ok();
                                }

                                let pipe = gst::Pipeline::new();
                                watch_pipeline_sources(&pipe, route);
                                // 2b: legacy `uridecodebin` per branch. `concat` does the
                                // gapless switching, so we no longer need uridecodebin3 /
                                // about-to-finish. Legacy uridecodebin handles Tidal
                                // `data:application/dash+xml` URIs and works on GStreamer
                                // < 1.24.
                                let mut udb =
                                    gst::ElementFactory::make("uridecodebin").property("uri", &uri);
                                if is_dash {
                                    udb = udb
                                        .property("buffer-duration", 15_000_000_000i64)
                                        .property("use-buffering", true);
                                } else {
                                    udb = udb
                                        .property("buffer-duration", 5_000_000_000i64)
                                        .property("use-buffering", true);
                                }
                                let uridecodebin = udb
                                    .build()
                                    .map_err(|e| format!("Failed to create uridecodebin: {e}"))?;
                                // Per-branch upstream queue (C1): decouples the decoder from
                                // concat's gate so the next branch can pre-buffer ahead while
                                // the current track plays. With one branch it's a passthrough.
                                // 15s of decoded reservoir for slow-internet cushion.
                                let branch_queue = gst::ElementFactory::make("queue")
                                    .property("max-size-time", 15_000_000_000u64)
                                    .property("max-size-buffers", 0u32)
                                    .property("max-size-bytes", 0u32)
                                    .build()
                                    .map_err(|e| format!("Failed to create branch queue: {e}"))?;
                                // `concat` at the head of the chain. With a single sink pad it
                                // is a passthrough (identical signal path to the old direct
                                // chain); the gapless second branch attaches in 2b-A2.
                                let concat = gst::ElementFactory::make("concat")
                                    .name("gapless-concat")
                                    .build()
                                    .map_err(|e| format!("Failed to create concat: {e}"))?;
                                let audioconvert = gst::ElementFactory::make("audioconvert")
                                    .build()
                                    .map_err(|e| format!("Failed to create audioconvert: {e}"))?;
                                let audioresample = gst::ElementFactory::make("audioresample")
                                    .build()
                                    .map_err(|e| format!("Failed to create audioresample: {e}"))?;
                                let norm_vol = gst::ElementFactory::make("volume")
                                    .property("volume", current_norm_gain)
                                    .build()
                                    .map_err(|e| format!("Failed to create norm volume: {e}"))?;
                                let user_vol = gst::ElementFactory::make("volume")
                                    .property("volume", slider_to_amplitude(current_volume))
                                    .build()
                                    .map_err(|e| format!("Failed to create user volume: {e}"))?;
                                let sink = gst::ElementFactory::make("autoaudiosink")
                                    .build()
                                    .map_err(|e| format!("Failed to create autoaudiosink: {e}"))?;

                                pipe.add_many([
                                    &uridecodebin,
                                    &branch_queue,
                                    &concat,
                                    &audioconvert,
                                    &audioresample,
                                    &norm_vol,
                                    &user_vol,
                                    &sink,
                                ])
                                .map_err(|e| format!("Failed to add elements: {e}"))?;
                                // Static chain: concat → audioconvert → … → sink.
                                // (uridecodebin → branch_queue is dynamic via pad_added;
                                // branch_queue.src → concat sink_0 is linked below.)
                                gst::Element::link_many([
                                    &concat,
                                    &audioconvert,
                                    &audioresample,
                                    &norm_vol,
                                    &user_vol,
                                    &sink,
                                ])
                                .map_err(|e| format!("Failed to link chain: {e}"))?;

                                // N2: connect notify::active-pad BEFORE requesting sink_0.
                                // gstconcat fires `notify` synchronously inside
                                // request_pad_simple when current_sinkpad is NULL; the
                                // first-fire-suppressing counter absorbs that.
                                //
                                // 2b-A3 (C4): on every subsequent fire we gate on
                                // active-pad IDENTITY, not a bare counter. We read
                                // `concat.active-pad` and only treat it as a gapless
                                // advance when its peer's parent is the currently-prerolled
                                // next bin's `branch_queue`. A transition to `None` (final
                                // EOS) or to a stale/unknown pad is ignored. This makes the
                                // handler robust across attach/detach churn.
                                let notify_count = Arc::new(AtomicU32::new(0));
                                let next_bin_notify = Arc::clone(&next_bin);
                                let next_active_notify = Arc::clone(&next_active);
                                let cmd_tx_notify = cmd_tx_worker.clone();
                                concat.connect_notify(Some("active-pad"), move |concat, _pspec| {
                                    let prev = notify_count.fetch_add(1, Ordering::AcqRel);
                                    if prev == 0 {
                                        // Initial sink_0 activation from request_pad_simple.
                                        log::debug!("[gapless-diag] notify active-pad fire #1 (initial sink_0), suppressed");
                                        return;
                                    }
                                    // Read the new active pad. None → final EOS transition; ignore.
                                    let Some(active_pad) = concat.property::<Option<gst::Pad>>("active-pad")
                                    else {
                                        // Dump concat's sink-pad situation so we can tell WHY it
                                        // went terminal: did sink_1 exist at all (timing), and was
                                        // it linked / already-EOS?
                                        let pads: Vec<String> = concat
                                            .sink_pads()
                                            .into_iter()
                                            .map(|p| {
                                                let linked = p.peer().is_some();
                                                format!("{}(linked={linked})", p.name())
                                            })
                                            .collect();
                                        log::debug!(
                                            "[gapless-diag] notify active-pad fire #{} → None (terminal). concat sink pads: [{}]",
                                            prev + 1,
                                            pads.join(", ")
                                        );
                                        return;
                                    };
                                    log::debug!("[gapless-diag] notify active-pad fire #{} → pad {}", prev + 1, active_pad.name());
                                    // Identity: the active pad's peer (a queue src) must belong
                                    // to the prerolled next bin's branch_queue. This is the only
                                    // transition we treat as a gapless advance.
                                    let peer_parent =
                                        active_pad.peer().and_then(|p| p.parent_element());
                                    let is_next = {
                                        match next_bin_notify.lock() {
                                            Ok(guard) => guard.as_ref().is_some_and(|nb| {
                                                peer_parent
                                                    .as_ref()
                                                    .is_some_and(|parent| parent == &nb.branch_queue)
                                            }),
                                            Err(_) => false,
                                        }
                                    };
                                    if !is_next {
                                        // Stale pad / not our next bin → not an advance.
                                        log::debug!("[gapless-diag] notify active-pad: peer_parent does NOT match next_bin.branch_queue (next_bin present={}); not an advance", next_bin_notify.lock().map(|g| g.is_some()).unwrap_or(false));
                                        return;
                                    }
                                    log::debug!("[gapless-diag] notify active-pad: MATCH next bin → dispatching HandleGaplessAdvance");
                                    // Mark the advance in-flight (gates detach/replace, C5)
                                    // and hand off to the worker. Keep this minimal — we're
                                    // on the streaming thread.
                                    next_active_notify.store(true, Ordering::Release);
                                    let _ = cmd_tx_notify.send(AudioCommand::HandleGaplessAdvance);
                                });

                                // Request concat sink_0 and link the branch queue into it.
                                let concat_sink_0 = concat
                                    .request_pad_simple("sink_%u")
                                    .ok_or_else(|| "concat refused initial sink pad".to_string())?;
                                let queue_src = branch_queue
                                    .static_pad("src")
                                    .ok_or_else(|| "branch queue has no src pad".to_string())?;
                                queue_src
                                    .link(&concat_sink_0)
                                    .map_err(|e| format!("Failed to link queue→concat: {e}"))?;

                                if let Some(sink_pad) = audioconvert.static_pad("sink") {
                                    let cell = Arc::clone(&decoded_cell_thread);
                                    sink_pad.add_probe(
                                        gst::PadProbeType::EVENT_DOWNSTREAM,
                                        move |_pad, info| {
                                            if let Some(gst::PadProbeData::Event(ref event)) =
                                                info.data
                                            {
                                                if let gst::EventView::Caps(caps_event) =
                                                    event.view()
                                                {
                                                    let caps = caps_event.caps();
                                                    if let Some(fmt) = parse_pcm_format(caps) {
                                                        if let Ok(mut guard) = cell.lock() {
                                                            *guard = Some(
                                                                crate::pipeline_probe::PadCaps {
                                                                    format: fmt.gst_format.clone(),
                                                                    rate: fmt.sample_rate,
                                                                    channels: fmt.channels,
                                                                },
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            gst::PadProbeReturn::Ok
                                        },
                                    );
                                }

                                // autoaudiosink is a bin — its real child sink
                                // (pulsesink/pipewiresink/alsasink) is added asynchronously.
                                // Hook child-added to attach a CAPS probe on the real sink's pad.
                                // Race trade-off: if the child is added BEFORE this signal handler
                                // is connected, the initial CAPS event is missed and output_cell
                                // stays None until the next caps event (e.g., format renegotiation)
                                // or until the 2s heartbeat triggers a refresh; the diagram
                                // gracefully shows "—" until then. In practice the connect happens
                                // before the pipeline transitions to PAUSED, so the race is rare.
                                if let Ok(sink_bin) = sink.clone().dynamic_cast::<gst::Bin>() {
                                    let output_cell = Arc::clone(&output_cell_thread);
                                    sink_bin.connect_element_added(move |_bin, element| {
                                        let cell = Arc::clone(&output_cell);
                                        if let Some(sink_pad) = element.static_pad("sink") {
                                            sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                                                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                                                    if let gst::EventView::Caps(caps_event) = event.view() {
                                                        let caps = caps_event.caps();
                                                        if let Some(fmt) = parse_pcm_format(caps) {
                                                            if let Ok(mut guard) = cell.lock() {
                                                                *guard = Some(crate::pipeline_probe::PadCaps {
                                                                    format: fmt.gst_format.clone(),
                                                                    rate: fmt.sample_rate,
                                                                    channels: fmt.channels,
                                                                });
                                                            }
                                                        }
                                                    }
                                                }
                                                gst::PadProbeReturn::Ok
                                            });
                                        }
                                    });
                                }

                                // uridecodebin(A) → branch_queue (dynamic). The branch
                                // queue's src is already linked to concat sink_0.
                                let branch_queue_weak = branch_queue.downgrade();
                                uridecodebin.connect_pad_added(move |_src, src_pad| {
                                    let Some(branch_queue) = branch_queue_weak.upgrade() else {
                                        return;
                                    };
                                    let Some(sink_pad) = branch_queue.static_pad("sink") else {
                                        return;
                                    };
                                    if sink_pad.is_linked() {
                                        return;
                                    }
                                    if let Some(caps) = src_pad.current_caps() {
                                        if let Some(s) = caps.structure(0) {
                                            if !s.name().as_str().starts_with("audio/") {
                                                return;
                                            }
                                        }
                                    }
                                    if let Err(e) = src_pad.link(&sink_pad) {
                                        log::error!("Failed to link uridecodebin pad: {e:?}");
                                    }
                                });

                                pipe.set_state(gst::State::Playing)
                                    .map_err(|e| format!("Failed to start playback: {e}"))?;

                                // Bus watcher (normal mode). 2b: the StreamStart arm
                                // (2a gapless trigger) is gone; advance is driven by
                                // concat's notify::active-pad (2b-A3). The bus keeps
                                // Eos / Error / Buffering.
                                let eos_flag = Arc::clone(&eos);
                                let tearing_down_flag = Arc::clone(&tearing_down);
                                let app_handle_clone = app_handle.clone();
                                // 2b-A3: next-bin error isolation. The bus thread reads
                                // the shared next_bin to decide whether an Error message
                                // originated inside the prerolled next branch (parent
                                // walk). If so it forwards HandleNextBinError to the
                                // worker (which detaches it) and keeps the current track
                                // playing — NO audio-error, no teardown.
                                let next_bin_bus = Arc::clone(&next_bin);
                                let cmd_tx_bus = cmd_tx_worker.clone();
                                if let Some(bus) = pipe.bus() {
                                    std::thread::spawn(move || {
                                        for msg in bus.iter_timed(gst::ClockTime::NONE) {
                                            match msg.view() {
                                                gst::MessageView::Eos(..) => {
                                                    log::debug!("[gapless-diag] bus EOS (terminal) → emitting track-finished (concat did NOT switch to a next branch)");
                                                    eos_flag.store(true, Ordering::SeqCst);
                                                    if !tearing_down_flag.load(Ordering::SeqCst) {
                                                        app_handle_clone
                                                            .emit("track-finished", ())
                                                            .ok();
                                                    }
                                                    break;
                                                }
                                                gst::MessageView::Error(err) => {
                                                    let err_msg = err.error().to_string();
                                                    let debug_str = err
                                                        .debug()
                                                        .map(|s| s.to_string())
                                                        .unwrap_or_default();
                                                    log::error!(
                                                        "GStreamer error: {} (debug: {})",
                                                        err_msg,
                                                        debug_str
                                                    );

                                                    // Did this error originate inside the
                                                    // prerolled next bin? Walk the src's
                                                    // parent chain and compare against the
                                                    // next bin's element.
                                                    let is_next_bin_error = {
                                                        let next_el = next_bin_bus
                                                            .lock()
                                                            .ok()
                                                            .and_then(|g| {
                                                                g.as_ref().map(|nb| nb.bin.clone())
                                                            });
                                                        match (err.src(), next_el) {
                                                            (Some(src), Some(next_el)) => {
                                                                let mut cur: Option<gst::Object> =
                                                                    Some(src.clone());
                                                                let mut found = false;
                                                                while let Some(obj) = cur.take() {
                                                                    if let Ok(el) = obj
                                                                        .clone()
                                                                        .downcast::<gst::Element>(
                                                                    ) {
                                                                        if el == next_el {
                                                                            found = true;
                                                                            break;
                                                                        }
                                                                        cur = el.parent();
                                                                    } else {
                                                                        cur = obj.parent();
                                                                    }
                                                                }
                                                                found
                                                            }
                                                            _ => false,
                                                        }
                                                    };

                                                    if is_next_bin_error {
                                                        // Isolate: detach the bad next bin,
                                                        // keep the current track playing. The
                                                        // natural EOS will fall back to
                                                        // track-finished → playNext fresh.
                                                        log::warn!(
                                                            "[audio] gapless: next-bin bus error, isolating: {err_msg}"
                                                        );
                                                        let _ = cmd_tx_bus
                                                            .send(AudioCommand::HandleNextBinError);
                                                        // Do NOT set eos / emit audio-error /
                                                        // break — current track is unaffected.
                                                        continue;
                                                    }

                                                    eos_flag.store(true, Ordering::SeqCst);
                                                    if !tearing_down_flag.load(Ordering::SeqCst) {
                                                        let is_busy = err_msg.contains("busy")
                                                            || debug_str.contains("busy")
                                                            || err_msg.contains("EBUSY")
                                                            || debug_str.contains("EBUSY");
                                                        let kind = if is_busy {
                                                            "device_busy"
                                                        } else {
                                                            "playback_error"
                                                        };
                                                        app_handle_clone.emit("audio-error",
                                                            serde_json::json!({ "kind": kind, "message": err_msg })
                                                        ).ok();
                                                    }
                                                    break;
                                                }
                                                gst::MessageView::Buffering(b) => {
                                                    log::debug!(
                                                        "[audio] normal: buffering {}%",
                                                        b.percent()
                                                    );
                                                }
                                                _ => {}
                                            }
                                        }
                                    });
                                }

                                backend = Some(PlaybackBackend::Normal {
                                    pipeline: pipe,
                                    concat,
                                    user_volume_el: Some(user_vol),
                                    norm_volume_el: Some(norm_vol),
                                });

                                // 2b-A3: this is the sink_0 branch — the currently
                                // playing track. On a gapless advance it gets detached
                                // and replaced by the promoted next branch.
                                current_branch = Some((uridecodebin, branch_queue));
                            }

                            // Resume where the torn-down pipeline was. Only a
                            // rebuild passes `Some`, so an ordinary track start
                            // reaches none of this.
                            //
                            // The wait is the load-bearing half. Both paths above
                            // call `set_state(Playing)` and fall straight through,
                            // and a seek issued against a pipeline that has not
                            // prerolled is silently dropped — measured, not
                            // assumed — which looks exactly like this feature
                            // working while every track restarts at 0:00.
                            // `state()` returns once the asynchronous state change
                            // has settled, and that is the first moment a seek is
                            // accepted.
                            //
                            // It waits on a thread of its own because the worker is
                            // the sole receiver of its own command channel: waiting
                            // here froze play/pause, next/prev, seek, position
                            // polling and MPRIS for the whole bound — and against an
                            // unreachable proxy, which is precisely what enabling a
                            // proxy invites, it ran the bound out in full and the
                            // seek was refused anyway. Off-thread the bound costs
                            // the UI nothing, so it stays generous enough for a slow
                            // link to finish prerolling.
                            if let Some(position_secs) = start_secs {
                                let pos = gst::ClockTime::from_nseconds(
                                    (position_secs as f64 * 1_000_000_000.0) as u64,
                                );
                                let seek_to = move |pipeline: &gst::Pipeline| {
                                    let (ret, cur, pend) =
                                        pipeline.state(gst::ClockTime::from_seconds(10));
                                    log::debug!("[audio] resume: preroll {ret:?} {cur:?} {pend:?}");
                                    if let Err(e) = pipeline.seek_simple(
                                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                        pos,
                                    ) {
                                        log::warn!(
                                            "[audio] resume at {position_secs}s failed: {e}"
                                        );
                                    }
                                };
                                match backend.as_ref() {
                                    Some(PlaybackBackend::Normal { pipeline, .. }) => {
                                        // A strong ref, so a teardown that beats the
                                        // preroll only delays the final unref past
                                        // the NULL the worker already drove it to —
                                        // where the seek is a refused no-op.
                                        let pipeline = pipeline.clone();
                                        std::thread::spawn(move || seek_to(&pipeline));
                                    }
                                    Some(PlaybackBackend::HqPlayer { .. }) => {
                                        // The arm thread seeks the decoder before
                                        // capture. Seeking the live WAV would skip
                                        // inside a file HQPlayer is already reading.
                                    }
                                    Some(PlaybackBackend::DirectAlsa { pipeline, .. }) => {
                                        // The generation is bumped here, on the
                                        // worker, because it is worker-local state;
                                        // only the *publishing* of it waits for the
                                        // preroll, which is what makes the writer
                                        // discard the pre-seek chunks in flight.
                                        track_generation += 1;
                                        let resume_gen = track_generation;
                                        let pipeline = pipeline.clone();
                                        let writer_gen = Arc::clone(&writer_gen);
                                        let frames_written = Arc::clone(&frames_written);
                                        let sample_rate = Arc::clone(&current_sample_rate);
                                        let paused = Arc::clone(&paused);
                                        let writer_tx = writer_tx.clone();
                                        std::thread::spawn(move || {
                                            let (ret, cur, pend) =
                                                pipeline.state(gst::ClockTime::from_seconds(10));
                                            log::debug!(
                                                "[audio] resume: preroll {ret:?} {cur:?} {pend:?}"
                                            );
                                            // `fetch_max` never walks the generation
                                            // backwards: if a newer track claimed the
                                            // writer while this one prerolled, it owns
                                            // the position and the seek is abandoned.
                                            if writer_gen.fetch_max(resume_gen, Ordering::AcqRel)
                                                > resume_gen
                                            {
                                                return;
                                            }
                                            // `frames_written` is the only position
                                            // this backend reports, and the writer
                                            // has to be unblocked to take the Flush.
                                            let was_paused = paused.load(Ordering::Acquire);
                                            paused.store(false, Ordering::Release);
                                            if let Some(ref tx) = writer_tx {
                                                let _ = tx.send(WriterCommand::Flush);
                                            }
                                            let seek_frames = (position_secs as f64
                                                * sample_rate.load(Ordering::Relaxed) as f64)
                                                as u64;
                                            frames_written.store(seek_frames, Ordering::Relaxed);
                                            if let Err(e) = pipeline.seek_simple(
                                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                                pos,
                                            ) {
                                                log::warn!(
                                                    "[audio] resume at {position_secs}s failed: {e}"
                                                );
                                            }
                                            if was_paused {
                                                paused.store(true, Ordering::Release);
                                            }
                                        });
                                    }
                                    None => {}
                                }
                            }
                            Ok(())
                        })();
                        if result.is_ok() {
                            current_uri = Some(uri);
                            current_track_id = track_id;
                        }
                        if let (Ok(()), Some((started, playback_generation, feed, pipeline))) =
                            (&result, startup)
                        {
                            let app = app_handle.clone();
                            let watch = Arc::clone(&hq_watch);
                            let generation = hq_gen.load(Ordering::Acquire);
                            let generation_cell = Arc::clone(&hq_gen);
                            std::thread::spawn(move || {
                                let result = started
                                    .recv_timeout(std::time::Duration::from_secs(60))
                                    .unwrap_or_else(|_| {
                                        Err("HQPlayer did not confirm playback in time".into())
                                    });
                                if generation_cell.load(Ordering::Acquire) != generation {
                                    let _ = reply.send(Err("Playback superseded".into()));
                                    return;
                                }
                                if result.is_ok() {
                                    if !publish_hq_output(
                                        &app,
                                        &watch,
                                        Some(config),
                                        (&generation_cell, generation),
                                        playback_generation,
                                    ) {
                                        let _ = reply.send(Err("Playback superseded".into()));
                                        return;
                                    }
                                } else {
                                    feed.cancel();
                                    let _ = pipeline.set_state(gst::State::Null);
                                    publish_hq_output(
                                        &app,
                                        &watch,
                                        None,
                                        (&generation_cell, generation),
                                        playback_generation,
                                    );
                                }
                                let _ = reply.send(result);
                            });
                        } else {
                            if result.is_ok() {
                                publish_output_state(
                                    &app_handle,
                                    &output_state_thread,
                                    &output_transaction_thread,
                                    Some(config),
                                );
                            }
                            reply.send(result).ok();
                        }
                    }

                    AudioCommand::Pause { reply } => {
                        paused.store(true, Ordering::Release);
                        let result = match backend.as_ref() {
                            Some(PlaybackBackend::Normal { pipeline, .. }) => pipeline
                                .set_state(gst::State::Paused)
                                .map(|_| ())
                                .map_err(|e| format!("Failed to pause: {e}")),
                            Some(PlaybackBackend::DirectAlsa { pipeline, .. }) => {
                                paused.store(true, Ordering::Release);
                                pipeline
                                    .set_state(gst::State::Paused)
                                    .map(|_| ())
                                    .map_err(|e| format!("Failed to pause decode: {e}"))
                            }
                            Some(PlaybackBackend::HqPlayer {
                                pipeline, control, ..
                            }) => hq_send_transport(control, &hq_host, hq_port, |session| {
                                session.pause()
                            })
                            .and_then(|()| {
                                pipeline
                                    .set_state(gst::State::Paused)
                                    .map_err(|e| e.to_string())?;
                                paused.store(true, Ordering::Release);
                                Ok(())
                            }),
                            None => Err("No active pipeline".into()),
                        };
                        reply.send(result).ok();
                    }

                    AudioCommand::Resume { reply } => {
                        let result = match backend.as_ref() {
                            Some(PlaybackBackend::Normal { pipeline, .. }) => pipeline
                                .set_state(gst::State::Playing)
                                .map(|_| ())
                                .map_err(|e| format!("Failed to resume: {e}")),
                            Some(PlaybackBackend::DirectAlsa { pipeline, .. }) => {
                                paused.store(false, Ordering::Release);
                                pipeline
                                    .set_state(gst::State::Playing)
                                    .map(|_| ())
                                    .map_err(|e| format!("Failed to resume decode: {e}"))
                            }
                            Some(PlaybackBackend::HqPlayer {
                                pipeline, control, ..
                            }) => hq_send_transport(control, &hq_host, hq_port, |session| {
                                session.play()
                            })
                            .and_then(|()| {
                                pipeline
                                    .set_state(gst::State::Playing)
                                    .map_err(|e| e.to_string())?;
                                paused.store(false, Ordering::Release);
                                Ok(())
                            }),
                            None => Err("No active pipeline".into()),
                        };
                        if result.is_ok() {
                            paused.store(false, Ordering::Release);
                        }
                        reply.send(result).ok();
                    }

                    AudioCommand::Stop { reply } => {
                        if matches!(backend.as_ref(), Some(PlaybackBackend::HqPlayer { .. })) {
                            if let Err(error) = hq_send_stop(&hq_control, &hq_host, hq_port) {
                                let _ = reply.send(Err(error));
                                continue;
                            }
                        }

                        writer_cancel.store(true, Ordering::Release);
                        cancel_hq_prepared(&hq_watch, &hq_control, &hq_host, hq_port);
                        hq_watch.started_gen.store(0, Ordering::Release);
                        hq_watch.advancing.store(false, Ordering::Release);
                        // 2b-A3 (detach matrix): Stop tears down the whole pipeline,
                        // so the next-bin + current-branch elements die with it. Just
                        // null the gapless slots (no executor detach — moot, and it
                        // would race the impending set_state(Null)).
                        if let Ok(mut g) = next_bin.lock() {
                            *g = None;
                        }
                        next_active.store(false, Ordering::Release);
                        current_branch = None;
                        // Cleared with `has_uri` below: nothing is playing, so a
                        // later route change has nothing to resume.
                        current_uri = None;
                        let result = match backend.take() {
                            Some(PlaybackBackend::Normal { pipeline, .. }) => {
                                if let Some(bus) = pipeline.bus() {
                                    bus.set_flushing(true);
                                }
                                eos.store(false, Ordering::SeqCst);
                                has_uri.store(false, Ordering::SeqCst);
                                std::thread::spawn(move || {
                                    pipeline.set_state(gst::State::Null).ok();
                                });
                                *decoded_cell_thread.lock().unwrap() = None;
                                *output_cell_thread.lock().unwrap() = None;
                                Ok(())
                            }
                            Some(PlaybackBackend::DirectAlsa { pipeline, .. }) => {
                                // Bump generation so writer discards stale data,
                                // then unblock and shut down
                                paused.store(false, Ordering::Release);
                                track_generation += 1;
                                writer_gen.store(track_generation, Ordering::Release);
                                if let Some(bus) = pipeline.bus() {
                                    bus.set_flushing(true);
                                }
                                if let Some(tx) = writer_tx.take() {
                                    let _ = tx.send_timeout(
                                        WriterCommand::Shutdown,
                                        std::time::Duration::from_millis(200),
                                    );
                                }
                                pipeline.set_state(gst::State::Null).ok();
                                let _ = pipeline.state(gst::ClockTime::from_mseconds(500));
                                drop(pipeline);
                                if let Some(h) = writer_thread.take() {
                                    h.join().ok();
                                }
                                eos.store(false, Ordering::SeqCst);
                                has_uri.store(false, Ordering::SeqCst);
                                *decoded_cell_thread.lock().unwrap() = None;
                                *output_cell_thread.lock().unwrap() = None;
                                Ok(())
                            }
                            Some(PlaybackBackend::HqPlayer {
                                pipeline,
                                control,
                                feed,
                                finish_emit,
                                ..
                            }) => {
                                finish_emit.store(false, Ordering::SeqCst);
                                feed.cancel();
                                bump_hq_generation(&output_transaction_thread, &hq_gen);
                                let _ = control;
                                if let Some(bus) = pipeline.bus() {
                                    bus.set_flushing(true);
                                }
                                pipeline.set_state(gst::State::Null).ok();
                                let _ = pipeline.state(gst::ClockTime::from_mseconds(500));
                                drop(pipeline);
                                writer_cancel.store(true, Ordering::Release);
                                release_alsa_writer(
                                    &mut writer_tx,
                                    &mut writer_thread,
                                    &mut writer_fmt,
                                    &mut writer_supported_fmts,
                                    &mut writer_supported_rates,
                                    &mut writer_device,
                                    &mut writer_bit_perfect,
                                );
                                eos.store(false, Ordering::SeqCst);
                                has_uri.store(false, Ordering::SeqCst);
                                *decoded_cell_thread.lock().unwrap() = None;
                                *output_cell_thread.lock().unwrap() = None;
                                Ok(())
                            }
                            None => {
                                // Clean up orphaned writer (e.g. pipeline build failed after spawn)
                                if let Some(tx) = writer_tx.take() {
                                    let _ = tx.try_send(WriterCommand::Shutdown);
                                }
                                if let Some(h) = writer_thread.take() {
                                    h.join().ok();
                                }
                                Ok(())
                            }
                        };
                        if result.is_ok() {
                            publish_output_state(
                                &app_handle,
                                &output_state_thread,
                                &output_transaction_thread,
                                None,
                            );
                        }
                        reply.send(result).ok();
                    }

                    AudioCommand::SetVolume { level, reply } => {
                        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
                            let _ = reply.send(Err("Volume must be between 0 and 1".into()));
                            continue;
                        }
                        if backend.is_some()
                            && (bit_perfect
                                || matches!(
                                    backend.as_ref(),
                                    Some(PlaybackBackend::HqPlayer { .. })
                                ))
                        {
                            let _ = reply.send(Err(
                                "Volume is controlled by the active output route".into(),
                            ));
                            continue;
                        }
                        current_volume = level as f64;
                        let amplitude = slider_to_amplitude(current_volume);
                        if let Some(vol) = backend.as_ref().and_then(|b| b.user_volume_el()) {
                            vol.set_property("volume", amplitude);
                        }
                        combined_vol.store(
                            ((amplitude * current_norm_gain) as f32).to_bits(),
                            Ordering::Relaxed,
                        );
                        if !matches!(backend.as_ref(), Some(PlaybackBackend::HqPlayer { .. })) {
                            signal_path.set_user_volume(amplitude as f32);
                        }
                        reply.send(Ok(())).ok();
                    }

                    AudioCommand::SetNormalizationGain { gain, reply } => {
                        if backend.is_some()
                            && (bit_perfect
                                || matches!(
                                    backend.as_ref(),
                                    Some(PlaybackBackend::HqPlayer { .. })
                                ))
                        {
                            current_norm_gain = gain;
                            signal_path.set_norm_gain_factor(1.0);
                        } else {
                            apply_normalization_gain(
                                gain,
                                &mut current_norm_gain,
                                backend.as_ref().and_then(|b| b.norm_volume_el()),
                                &combined_vol,
                                current_volume,
                                &signal_path,
                            );
                        }
                        reply.send(Ok(())).ok();
                    }

                    AudioCommand::Seek {
                        position_secs,
                        reply,
                    } => {
                        if matches!(backend.as_ref(), Some(PlaybackBackend::HqPlayer { .. })) {
                            if let Some(uri) = current_uri.clone() {
                                let _ = cmd_tx_worker.send(AudioCommand::PlayUrl {
                                    uri,
                                    start_secs: Some(position_secs),
                                    preserve_output: true,
                                    track_id: current_track_id,
                                    resume_paused: paused.load(Ordering::Acquire),
                                    reply,
                                });
                            } else {
                                let _ = reply.send(Err("No active track".into()));
                            }
                            continue;
                        }
                        // 2b-A3 (detach matrix): a flush seek must NOT detach the
                        // prerolled next branch. Empirically verified on GStreamer
                        // 1.24.2 (python-gi, concat + dual uridecodebin→queue): a
                        // `seek_simple(FLUSH|KEY_UNIT)` on the pipeline forwards the
                        // FLUSH_START/FLUSH_STOP + new SEGMENT only to concat's ACTIVE
                        // sink pad (sink_0). The inactive prerolled branch on sink_1
                        // sees ZERO flush events, stays linked + PLAYING, and concat
                        // still switches to it at the active branch's EOS (gapless
                        // advance intact). Detaching here was the bug: it threw away a
                        // valid preroll on every seek, forcing a frontend rebuild (gap)
                        // at the boundary. Concat re-bases running time per active
                        // segment, so the seek touches only the current track; the next
                        // branch's relationship to concat is unaffected. Leave it armed.
                        let result = match backend.as_ref() {
                            Some(PlaybackBackend::Normal { pipeline, .. }) => {
                                let pos = gst::ClockTime::from_nseconds(
                                    (position_secs as f64 * 1_000_000_000.0) as u64,
                                );
                                pipeline
                                    .seek_simple(
                                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                        pos,
                                    )
                                    .map_err(|e| format!("Seek failed: {e}"))
                            }
                            Some(PlaybackBackend::DirectAlsa { pipeline, .. }) => {
                                let was_paused = paused.load(Ordering::Acquire);
                                paused.store(false, Ordering::Release);
                                track_generation += 1;
                                writer_gen.store(track_generation, Ordering::Release);
                                if let Some(ref tx) = writer_tx {
                                    let _ = tx.send(WriterCommand::Flush);
                                }
                                let pos = gst::ClockTime::from_nseconds(
                                    (position_secs as f64 * 1_000_000_000.0) as u64,
                                );
                                let seek_frames = (position_secs as f64
                                    * current_sample_rate.load(Ordering::Relaxed) as f64)
                                    as u64;
                                frames_written.store(seek_frames, Ordering::Relaxed);
                                let result = pipeline
                                    .seek_simple(
                                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                        pos,
                                    )
                                    .map_err(|e| format!("Seek failed: {e}"));
                                if was_paused {
                                    paused.store(true, Ordering::Release);
                                }
                                result
                            }
                            Some(PlaybackBackend::HqPlayer { .. }) => {
                                unreachable!("HQ seek is rebuilt above")
                            }
                            None => Err("No active pipeline".into()),
                        };
                        reply.send(result).ok();
                    }

                    AudioCommand::GetPosition { reply } => {
                        let pos = match backend.as_ref() {
                            Some(PlaybackBackend::Normal { pipeline, .. }) => pipeline
                                .query_position::<gst::ClockTime>()
                                // 2b (C2): under concat (adjust-base=true) query_position
                                // re-bases per track, so it is already B-relative — no
                                // offset subtraction needed.
                                .map(|pos| pos.nseconds() as f32 / 1_000_000_000.0)
                                .unwrap_or(0.0),
                            Some(PlaybackBackend::DirectAlsa { .. }) => {
                                let frames = frames_written.load(Ordering::Relaxed);
                                let rate = current_sample_rate.load(Ordering::Relaxed);
                                if rate > 0 {
                                    frames as f32 / rate as f32
                                } else {
                                    0.0
                                }
                            }
                            Some(PlaybackBackend::HqPlayer { heard, .. }) => {
                                // The watcher writes this. A Status roundtrip
                                // here would stall pause, stop, and the next
                                // track on the control port.
                                f32::from_bits(heard.load(Ordering::Relaxed))
                            }
                            None => 0.0,
                        };
                        reply.send(Ok(pos)).ok();
                    }

                    AudioCommand::IsFinished { reply } => {
                        let finished =
                            eos.load(Ordering::SeqCst) || !has_uri.load(Ordering::SeqCst);
                        reply.send(Ok(finished)).ok();
                    }

                    AudioCommand::OutputChanged => {
                        let stale = {
                            let mut guard = next_bin.lock().unwrap();
                            let previous_generation =
                                route_generation.fetch_add(1, Ordering::AcqRel);
                            // Output changes cancel in-flight attaches without changing
                            // the active pipeline's proxy hook. Keep a current hook
                            // current so a rolled-back preference can preroll again.
                            // Never bless a hook already made stale by a proxy save.
                            if pipeline_route_generation == previous_generation {
                                pipeline_route_generation = previous_generation + 1;
                            }
                            if next_active.load(Ordering::Acquire) {
                                None
                            } else {
                                guard.take()
                            }
                        };
                        if let (
                            Some(stale),
                            Some(PlaybackBackend::Normal {
                                pipeline, concat, ..
                            }),
                        ) = (stale, backend.as_ref())
                        {
                            let _ = attach_tx.send(AttachJob::Detach {
                                pipeline: pipeline.clone(),
                                concat: concat.clone(),
                                bin: stale.bin,
                                branch_queue: stale.branch_queue,
                            });
                        }
                        if !hq_watch.advancing.load(Ordering::Acquire) {
                            cancel_hq_for_output_change(&hq_watch, &hq_control, &hq_host, hq_port);
                        }
                    }

                    AudioCommand::SetProxySettings { settings, reply } => {
                        let probed = probe_host_caps();
                        // Kept to answer one question below: does this change alter
                        // what the audio thread may do? Taken by replacement so the
                        // cell is never briefly empty.
                        let previous = {
                            let mut ap = audio_proxy
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            std::mem::replace(&mut *ap, AudioProxy::new(settings, probed))
                        };

                        {
                            // Promotion follows whichever tier is routable. On 1.24
                            // with credentials that is Dash, so curlhttpsrc is
                            // promoted process-wide on a host where its progressive
                            // seek is broken -- harmless only because Lossy refuses
                            // separately at the build site. The promotion and the
                            // refusal are load-bearing for each other.
                            let ap = audio_proxy.lock().unwrap_or_else(|p| p.into_inner());
                            let probe_route = ap
                                .route_for(crate::proxy::Capability::Lossy)
                                .or_else(|_| ap.route_for(crate::proxy::Capability::Dash))
                                .unwrap_or(crate::proxy::Route::NoProxy);
                            promote_curl_source(&probe_route, original_curl_rank);
                        }

                        // Detach unconditionally. A branch prerolled under the
                        // previous settings already holds an open source with up
                        // to fifteen seconds buffered; left alone it becomes the
                        // next playing track, still routed the old way.
                        //
                        // Deliberately not conditional on "did the route change".
                        // Comparing routes cannot see off -> blocked (both would
                        // have to be spelled the same), and a spurious detach
                        // costs one re-preroll and nothing else.
                        //
                        // The bump and the take are one critical section on the
                        // `next_bin` mutex, and the executor reads that generation
                        // and stores under the same one. Split apart, the executor
                        // can store a branch built under the old route immediately
                        // after this take found the slot empty — and nothing
                        // downstream would ever refuse it: the gapless advance that
                        // promotes it reaches no Tauri command, and `SetNextTrack`'s
                        // dedup makes it sticky once it is armed.
                        let stale = {
                            let mut guard = match next_bin.lock() {
                                Ok(g) => g,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            route_generation.fetch_add(1, Ordering::AcqRel);
                            // Mid-advance the slot belongs to HandleGaplessAdvance
                            // (C5) — the generation still has to move, so the bump
                            // sits outside this gate.
                            if next_active.load(Ordering::Acquire) {
                                None
                            } else {
                                guard.take()
                            }
                        };
                        if let (
                            Some(stale),
                            Some(PlaybackBackend::Normal {
                                pipeline, concat, ..
                            }),
                        ) = (stale, backend.as_ref())
                        {
                            let _ = attach_tx.send(AttachJob::Detach {
                                pipeline: pipeline.clone(),
                                concat: concat.clone(),
                                bin: stale.bin,
                                branch_queue: stale.branch_queue,
                            });
                        }

                        // The playing pipeline carries the route it was built with:
                        // `watch_pipeline_sources` snapshotted it by value, and no
                        // later command re-applies one. So the only way to stop a
                        // track streaming on the previous route is to rebuild it —
                        // which is also what replaces the stale hook, and with it
                        // every branch that would have been attached under it.
                        //
                        // Gated on `backend.is_some()`, not on a play/pause flag: a
                        // paused pipeline still holds an open source.
                        //
                        // `eos` is the one exception, and it is not a play/pause
                        // flag. FOUR bus handlers set it — the terminal EOS arm and
                        // the Error arm of each mode's watcher — and it is cleared
                        // only where a track starts or playback is torn down:
                        // `PlayUrl`, `HandleGaplessAdvance`, and both backend arms
                        // of `Stop`. So it means playback ended, by completion or by
                        // failure; it does NOT mean the source drained.
                        //
                        // The failure shape is the one that reaches here, and it is
                        // the containment hole. An Error arm sets `eos`, emits
                        // `audio-error` and breaks WITHOUT tearing anything down, so
                        // the pipeline is left standing in PLAYING with
                        // `backend = Some` and a source still open on the route it
                        // was built with; the user gets a toast and nothing else
                        // moves. Change the route now and that source keeps its old
                        // one for as long as the backend lives.
                        //
                        // The clean-EOS shape barely exists by comparison: the bus
                        // emits `track-finished`, the frontend's listener calls
                        // `playNext`, and `playNext` invokes `stop_track` before it
                        // inspects the queue at all, so that backend is gone within
                        // milliseconds. (Repeat-one returns above that call, and it
                        // re-plays immediately, which clears `eos`.)
                        //
                        // Rebuilding is the wrong answer in either shape: it would
                        // re-open the stream, seek to the duration, EOS again and
                        // emit a second `track-finished` — which the frontend turns
                        // into `playNext`, i.e. autoplay radio starting while the UI
                        // reads stopped. Driving the pipeline to NULL is what
                        // actually releases the stale-route source, and it resumes
                        // nothing.
                        let current = audio_proxy
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let route_changed = audio_route_differs(&previous, &current);
                        drop(current);
                        if !route_changed {
                            // No rebuild is coming, and the hook on the live
                            // pipeline still carries what a fresh build would
                            // compute — so the bump above did not make it stale.
                            // Without this, a save that changes nothing would
                            // refuse every later preroll on the playing track.
                            pipeline_route_generation = route_generation.load(Ordering::Acquire);
                        }
                        if route_changed && backend.is_some() {
                            if eos.load(Ordering::SeqCst) {
                                // Queued on the self-sender for the same reason the
                                // rebuild below is re-issued that way: this arm is
                                // an inline closure, and `Stop` is another arm of
                                // the same match rather than a function.
                                //
                                // It lands at the BACK of the worker queue, so a
                                // user-initiated `PlayUrl` enqueued while this arm
                                // was running is processed first and this `Stop`
                                // then tears down the track it just started, with
                                // the UI reading playing. Milliseconds wide, and
                                // the same class of window the rebuild path below
                                // already carries — named here, not guarded.
                                let (stop_tx, _stop_rx) = mpsc::channel();
                                let _ = cmd_tx_worker.send(AudioCommand::Stop { reply: stop_tx });
                            } else if let Some(uri) = current_uri.clone() {
                                // `get_position` cannot be called here — it is an
                                // `AudioPlayer` method, and this thread is the one
                                // its reply would have to come back through. This is
                                // the body of the `GetPosition` arm.
                                let position_secs = match backend.as_ref() {
                                    Some(PlaybackBackend::Normal { pipeline, .. }) => pipeline
                                        .query_position::<gst::ClockTime>()
                                        .map(|pos| pos.nseconds() as f32 / 1_000_000_000.0)
                                        .unwrap_or(0.0),
                                    Some(PlaybackBackend::DirectAlsa { .. }) => {
                                        let frames = frames_written.load(Ordering::Relaxed);
                                        let rate = current_sample_rate.load(Ordering::Relaxed);
                                        if rate > 0 {
                                            frames as f32 / rate as f32
                                        } else {
                                            0.0
                                        }
                                    }
                                    Some(PlaybackBackend::HqPlayer { heard, .. }) => {
                                        f32::from_bits(heard.load(Ordering::Relaxed))
                                    }
                                    None => 0.0,
                                };
                                log::info!(
                                    "[proxy] route changed mid-track: rebuilding at {position_secs:.1}s"
                                );
                                // There is no `PlayUrl` function to call — the arm
                                // is an inline closure — so re-issue it as a command
                                // on the self-sender. The reply is watched off-thread
                                // purely so a refusal under the new settings reaches
                                // the user rather than stopping playback in silence.
                                let (rebuilt_tx, rebuilt_rx) =
                                    mpsc::channel::<Result<(), String>>();
                                let app_handle_rebuild = app_handle.clone();
                                std::thread::spawn(move || {
                                    if let Ok(Err(e)) = rebuilt_rx.recv() {
                                        log::warn!("[proxy] rebuild after route change: {e}");
                                        app_handle_rebuild
                                            .emit(
                                                "audio-error",
                                                serde_json::json!({
                                                    "kind": "playback_error",
                                                    "message": e,
                                                }),
                                            )
                                            .ok();
                                    }
                                });
                                // A paused track is torn down like any other (it
                                // holds an open source), but it must not come back
                                // playing: the frontend still says paused, and the
                                // two would disagree out loud. Queued behind the
                                // rebuild, so it pauses the pipeline that replaces
                                // this one.
                                let resume_paused = match backend.as_ref() {
                                    Some(PlaybackBackend::Normal { pipeline, .. }) => {
                                        // `current_state` alone would read a pipeline
                                        // still prerolling toward PLAYING as paused.
                                        let (_, cur, pending) =
                                            pipeline.state(gst::ClockTime::ZERO);
                                        cur == gst::State::Paused
                                            && pending == gst::State::VoidPending
                                    }
                                    Some(PlaybackBackend::DirectAlsa { .. }) => {
                                        paused.load(Ordering::Acquire)
                                    }
                                    Some(PlaybackBackend::HqPlayer { pipeline, .. }) => {
                                        let (_, cur, pending) =
                                            pipeline.state(gst::ClockTime::ZERO);
                                        cur == gst::State::Paused
                                            && pending == gst::State::VoidPending
                                    }
                                    None => false,
                                };
                                let _ = cmd_tx_worker.send(AudioCommand::PlayUrl {
                                    uri,
                                    start_secs: Some(position_secs),
                                    preserve_output: true,
                                    track_id: current_track_id,
                                    resume_paused,
                                    reply: rebuilt_tx,
                                });
                                if resume_paused {
                                    let (paused_tx, _paused_rx) = mpsc::channel();
                                    let _ = cmd_tx_worker
                                        .send(AudioCommand::Pause { reply: paused_tx });
                                }
                            }
                        }

                        reply.send(()).ok();
                    }

                    AudioCommand::SetGapless { enabled, reply } => {
                        // 2b-A2: drives SetNextTrack's effective-gapless gate.
                        gapless_setting = enabled;
                        if !enabled {
                            cancel_hq_for_output_change(&hq_watch, &hq_control, &hq_host, hq_port);
                        }
                        // 2b-A3 (detach matrix): disabling gapless invalidates any
                        // prerolled next bin. Detach it (gated on !next_active).
                        if !enabled && !next_active.load(Ordering::Acquire) {
                            if let (
                                Some(stale),
                                Some(PlaybackBackend::Normal {
                                    pipeline, concat, ..
                                }),
                            ) = (
                                next_bin.lock().ok().and_then(|mut g| g.take()),
                                backend.as_ref(),
                            ) {
                                let _ = attach_tx.send(AttachJob::Detach {
                                    pipeline: pipeline.clone(),
                                    concat: concat.clone(),
                                    bin: stale.bin,
                                    branch_queue: stale.branch_queue,
                                });
                            }
                        }
                        let _ = reply.send(Ok(()));
                    }

                    // 2b-A2: preroll the next track via a second uridecodebin
                    // attached to concat sink_1, OFF the worker thread (C3). The
                    // worker only validates gating + dedup + records intent, then
                    // dispatches to the attach executor and replies immediately.
                    AudioCommand::SetNextTrack {
                        uri,
                        norm_gain,
                        track_id,
                        qid,
                        replay_gain,
                        peak_amplitude,
                        is_dash,
                        reply,
                    } => {
                        if output_state_thread.lock().unwrap().pending {
                            let _ = reply.send(Ok(()));
                            continue;
                        }

                        // HQPlayer gapless ignores the saved exclusive flags: the
                        // mode leaves them set and still owns the handoff.
                        if hq_enabled.load(Ordering::Acquire) && gapless_setting {
                            let playing =
                                matches!(backend.as_ref(), Some(PlaybackBackend::HqPlayer { .. }));
                            if !playing || hq_watch.advancing.load(Ordering::Acquire) {
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            {
                                let mut guard = hq_watch
                                    .next
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if let Some(existing) = guard.as_mut() {
                                    if existing.track_id == track_id {
                                        existing.qid = qid;
                                        drop(guard);
                                        let _ = reply.send(Ok(()));
                                        continue;
                                    }
                                }
                            }
                            if hq_watch
                                .preparing
                                .lock()
                                .unwrap()
                                .as_ref()
                                .is_some_and(|pending| {
                                    pending.track_id == track_id && pending.qid == qid
                                })
                            {
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            cancel_hq_prepared(&hq_watch, &hq_control, &hq_host, hq_port);
                            let token = hq_watch.next_gen.load(Ordering::Acquire);
                            let route = {
                                let ap = audio_proxy
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                ap.route_for(capability_of(is_dash))
                            };
                            let route = match route {
                                Ok(route) => route,
                                Err(blocked) => {
                                    log::warn!(
                                        "[hqplayer] next track not queued: {}",
                                        blocked.cause
                                    );
                                    let _ = reply.send(Ok(()));
                                    continue;
                                }
                            };
                            if matches!(route, crate::proxy::Route::Via { .. })
                                && !http_source_is_configurable()
                            {
                                log::warn!(
                                    "[hqplayer] next track not queued: https source \
                                     cannot be pointed at a proxy"
                                );
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            let gen = hq_gen.load(Ordering::Acquire);
                            let queued_uri = uri.clone();
                            match launch_hqplayer_track(
                                &uri,
                                is_dash,
                                route,
                                0.0,
                                gen,
                                Arc::clone(&hq_gen),
                                Arc::clone(&hq_control),
                                hq_host.clone(),
                                hq_port,
                                app_handle.clone(),
                                Arc::clone(&tearing_down),
                                Arc::clone(&eos),
                                Arc::clone(&signal_path),
                                Arc::clone(&decoded_cell_thread),
                                Arc::clone(&output_cell_thread),
                                Arc::clone(&hq_watch),
                                Some(HqQueueMeta {
                                    token,
                                    uri: queued_uri,
                                    track_id,
                                    qid,
                                    norm_gain,
                                    replay_gain,
                                    peak_amplitude,
                                }),
                                false,
                                Some(track_id),
                                Arc::clone(&paused),
                            ) {
                                Ok(_) => {
                                    let _ = reply.send(Ok(()));
                                }
                                Err(err) => {
                                    log::warn!("[hqplayer] next track not queued: {err}");
                                    let _ = reply.send(Err(err));
                                }
                            }
                            continue;
                        }

                        // Effective gapless = setting on AND normal mode (C5: never
                        // attach a concat branch under exclusive/bit-perfect — the
                        // DirectAlsa path has no concat).
                        let effective_gapless = gapless_setting
                            && !exclusive
                            && !bit_perfect
                            && !hq_enabled.load(Ordering::Acquire);

                        // Normal-mode pipeline/concat clones for the executor. If the
                        // backend isn't Normal (or absent), we can't (and mustn't,
                        // C5) touch concat — treat as "no preroll".
                        let normal_clones = match backend.as_ref() {
                            Some(PlaybackBackend::Normal {
                                pipeline, concat, ..
                            }) => Some((pipeline.clone(), concat.clone())),
                            _ => None,
                        };
                        log::debug!(
                            "[gapless-diag] SetNextTrack track={track_id}: effective_gapless={effective_gapless} (setting={gapless_setting} excl={exclusive} bp={bit_perfect} hq={}), backend_normal={}",
                            hq_enabled.load(Ordering::Acquire),
                            normal_clones.is_some()
                        );

                        if !effective_gapless || normal_clones.is_none() {
                            // Gapless off / wrong mode: detach any existing next_bin
                            // (gated on !next_active per C5) and do nothing else.
                            if !next_active.load(Ordering::Acquire) {
                                if let Some(stale) = next_bin.lock().ok().and_then(|mut g| g.take())
                                {
                                    if let Some(PlaybackBackend::Normal {
                                        pipeline, concat, ..
                                    }) = backend.as_ref()
                                    {
                                        let _ = attach_tx.send(AttachJob::Detach {
                                            pipeline: pipeline.clone(),
                                            concat: concat.clone(),
                                            bin: stale.bin,
                                            branch_queue: stale.branch_queue,
                                        });
                                    }
                                    // If not Normal we can't detach via concat; the
                                    // bin was attached under Normal so backend is
                                    // Normal here in practice. Drop silently otherwise.
                                }
                            }
                            let _ = reply.send(Ok(()));
                            continue;
                        }

                        let (pipeline, concat) = normal_clones.unwrap();

                        // Dedup (C5): if an existing next_bin targets the same track,
                        // just refresh its stored qid (so the eventual track-advanced
                        // payload matches the frontend queue) — no re-attach.
                        {
                            let mut guard = next_bin.lock().unwrap();
                            if let Some(existing) = guard.as_mut() {
                                if existing.track_id == track_id {
                                    existing.qid = qid.clone();
                                    drop(guard);
                                    let _ = reply.send(Ok(()));
                                    continue;
                                }
                            }
                        }

                        // A different next track is requested. Only replace if concat
                        // isn't already switching (C5) — otherwise let 2b-A3's advance
                        // complete and skip.
                        if next_active.load(Ordering::Acquire) {
                            let _ = reply.send(Ok(()));
                            continue;
                        }

                        // Detach the stale (different) bin, then attach the new one.
                        // Both jobs are serialized on the executor, so detach-then-
                        // attach is ordered correctly.
                        if let Some(stale) = next_bin.lock().ok().and_then(|mut g| g.take()) {
                            let _ = attach_tx.send(AttachJob::Detach {
                                pipeline: pipeline.clone(),
                                concat: concat.clone(),
                                bin: stale.bin,
                                branch_queue: stale.branch_queue,
                            });
                        }

                        log::debug!(
                            "[gapless-diag] SetNextTrack: dispatching ATTACH for track {track_id}"
                        );
                        let _ = attach_tx.send(AttachJob::Attach {
                            pipeline,
                            concat,
                            build_generation: pipeline_route_generation,
                            uri,
                            is_dash,
                            track_id,
                            qid,
                            norm_gain,
                            replay_gain,
                            peak_amplitude,
                        });
                        let _ = reply.send(Ok(()));
                    }

                    // 2b-A2: detach the prerolled next bin (gated on !next_active per
                    // C5 — if concat is already switching, leave it for 2b-A3).
                    AudioCommand::ClearNextTrack { reply } => {
                        cancel_hq_for_output_change(&hq_watch, &hq_control, &hq_host, hq_port);
                        if !next_active.load(Ordering::Acquire) {
                            if let Some(stale) = next_bin.lock().ok().and_then(|mut g| g.take()) {
                                if let Some(PlaybackBackend::Normal {
                                    pipeline, concat, ..
                                }) = backend.as_ref()
                                {
                                    let _ = attach_tx.send(AttachJob::Detach {
                                        pipeline: pipeline.clone(),
                                        concat: concat.clone(),
                                        bin: stale.bin,
                                        branch_queue: stale.branch_queue,
                                    });
                                }
                            }
                        }
                        let _ = reply.send(Ok(()));
                    }

                    // 2b-A3: concat switched its active pad to the prerolled next
                    // branch (verified by the notify identity gate). Run the
                    // now-playing cascade: detach the finished branch, promote the
                    // next branch to current, apply its normalization gain, and emit
                    // `track-advanced`. Fieldless (C6): all data comes from the
                    // single `next_bin` slot, which we `take()` — the empty-take
                    // guard makes a double-advance a no-op.
                    AudioCommand::HandleGaplessAdvance => {
                        let promoted = match next_bin.lock().ok().and_then(|mut g| g.take()) {
                            Some(p) => p,
                            None => {
                                // Spurious / double advance (e.g. a stray notify, or
                                // the slot was already taken/cleared). Release the gate
                                // and ignore.
                                log::debug!(
                                    "[audio] gapless: HandleGaplessAdvance with empty next_bin — ignoring"
                                );
                                next_active.store(false, Ordering::Release);
                                continue;
                            }
                        };

                        // Detach the now-finished current branch (sink_0). Safe to
                        // detach now that concat has switched away from it. Gated to
                        // Normal (C5) — the worker never reaches here on DirectAlsa
                        // (gapless is mode-gated off), but be defensive.
                        if let (
                            Some((old_bin, old_queue)),
                            Some(PlaybackBackend::Normal {
                                pipeline, concat, ..
                            }),
                        ) = (current_branch.take(), backend.as_ref())
                        {
                            let _ = attach_tx.send(AttachJob::Detach {
                                pipeline: pipeline.clone(),
                                concat: concat.clone(),
                                bin: old_bin,
                                branch_queue: old_queue,
                            });
                        }

                        // Promote the next branch to current. The URI moves with
                        // it: this branch is the playing track now, and a route
                        // change from here must rebuild *this* one.
                        current_branch = Some((promoted.bin, promoted.branch_queue));
                        current_uri = Some(promoted.uri);
                        current_track_id = Some(promoted.track_id);

                        // Apply the promoted track's normalization gain across the
                        // shared volume chain (concat is upstream of norm_vol, so the
                        // gain applies to the now-active branch).
                        apply_normalization_gain(
                            promoted.norm_gain,
                            &mut current_norm_gain,
                            backend.as_ref().and_then(|b| b.norm_volume_el()),
                            &combined_vol,
                            current_volume,
                            &signal_path,
                        );
                        signal_path.reset_for_track();

                        // Emit track-advanced (unchanged payload, C4). The lib.rs
                        // listener stores rg/peak + scrobbles; the frontend reconciles
                        // its queue by trackId/qid.
                        let _ = app_handle.emit(
                            "track-advanced",
                            serde_json::json!({
                                "trackId": promoted.track_id,
                                "qid": promoted.qid,
                                "replayGain": promoted.replay_gain,
                                "peakAmplitude": promoted.peak_amplitude,
                            }),
                        );

                        // A new track is now playing on the same pipeline: clear EOS,
                        // keep has_uri true. The boundary is resolved.
                        eos.store(false, Ordering::SeqCst);
                        has_uri.store(true, Ordering::SeqCst);
                        next_active.store(false, Ordering::Release);
                    }

                    AudioCommand::HandleHqAdvance => {
                        let prepared = match hq_watch.next.lock() {
                            Ok(mut guard) => guard.take(),
                            Err(poisoned) => poisoned.into_inner().take(),
                        };
                        let Some(prepared) = prepared.filter(|item| item.sent) else {
                            hq_watch.advancing.store(false, Ordering::Release);
                            continue;
                        };
                        match backend.take() {
                            Some(PlaybackBackend::HqPlayer {
                                pipeline,
                                feed,
                                finish_emit,
                                ..
                            }) => {
                                finish_emit.store(false, Ordering::SeqCst);
                                feed.cancel();
                                let _ = pipeline.set_state(gst::State::Null);
                            }
                            other => {
                                backend = other;
                                prepared.finish_emit.store(false, Ordering::SeqCst);
                                prepared.feed.cancel();
                                let _ = prepared.pipeline.set_state(gst::State::Null);
                                hq_watch.advancing.store(false, Ordering::Release);
                                continue;
                            }
                        }
                        let HqPrepared {
                            playback_generation,
                            pipeline,
                            feed,
                            heard,
                            finish_emit,
                            uri,
                            track_id,
                            qid,
                            norm_gain,
                            replay_gain,
                            peak_amplitude,
                            rate,
                            channels,
                            sent: _,
                        } = prepared;
                        signal_path.reset_for_track();
                        signal_path.set_backend("HQPlayer", None);
                        signal_path.set_audio_modes(false, false);
                        current_norm_gain = norm_gain;
                        signal_path.set_user_volume(1.0);
                        signal_path.set_norm_gain_factor(1.0);
                        signal_path.set_decoded("S32LE", rate, channels);
                        signal_path.set_output("S32LE", rate, channels);
                        let caps = crate::pipeline_probe::PadCaps {
                            format: "S32LE".to_string(),
                            rate,
                            channels,
                        };
                        if let Ok(mut guard) = decoded_cell_thread.lock() {
                            *guard = Some(caps.clone());
                        }
                        if let Ok(mut guard) = output_cell_thread.lock() {
                            *guard = Some(caps);
                        }
                        current_uri = Some(uri);
                        current_track_id = Some(track_id);
                        let active_config = output_state_thread.lock().unwrap().active.clone();
                        publish_hq_output(
                            &app_handle,
                            &hq_watch,
                            active_config,
                            (&hq_gen, hq_gen.load(Ordering::Acquire)),
                            playback_generation,
                        );
                        eos.store(false, Ordering::SeqCst);
                        has_uri.store(true, Ordering::SeqCst);
                        let _ = app_handle.emit(
                            "track-advanced",
                            serde_json::json!({
                                "trackId": track_id,
                                "qid": qid,
                                "replayGain": replay_gain,
                                "peakAmplitude": peak_amplitude,
                            }),
                        );
                        // Same generation as the track that just finished. A new
                        // PlayUrl is what bumps it.
                        let gen = hq_gen.load(Ordering::Acquire);
                        *hq_watch.last_status.lock().unwrap() = None;
                        let arm = HqArm {
                            playback_generation,
                            startup: Arc::new(Mutex::new(None)),
                            resume_paused: false,
                            track_id: Some(track_id),
                            paused: Arc::clone(&paused),
                            pipeline: pipeline.clone(),
                            feed: feed.clone(),
                            control: Arc::clone(&hq_control),
                            gen,
                            gen_cell: Arc::clone(&hq_gen),
                            origin: 0.0,
                            heard: Arc::clone(&heard),
                            finish_emit: Arc::clone(&finish_emit),
                            eos: Arc::clone(&eos),
                            reported: Arc::new(AtomicBool::new(false)),
                            app: app_handle.clone(),
                            signal: Arc::clone(&signal_path),
                            decoded_cell: Arc::clone(&decoded_cell_thread),
                            output_cell: Arc::clone(&output_cell_thread),
                            host: hq_host.clone(),
                            port: hq_port,
                            tearing_down: Arc::clone(&tearing_down),
                            watch: Arc::clone(&hq_watch),
                            queue: None,
                        };
                        backend = Some(PlaybackBackend::HqPlayer {
                            pipeline,
                            control: Arc::clone(&hq_control),
                            feed,
                            heard,
                            finish_emit,
                        });
                        hq_watch.advancing.store(false, Ordering::Release);
                        spawn_hq_watcher(arm, true);
                    }

                    // 2b-A3: a prerolled next bin reported a decode error on the bus.
                    // Detach it (gated on !next_active per C5) without touching the
                    // current track — the natural boundary falls back to playNext.
                    AudioCommand::HandleNextBinError => {
                        if !next_active.load(Ordering::Acquire) {
                            if let (
                                Some(stale),
                                Some(PlaybackBackend::Normal {
                                    pipeline, concat, ..
                                }),
                            ) = (
                                next_bin.lock().ok().and_then(|mut g| g.take()),
                                backend.as_ref(),
                            ) {
                                let _ = attach_tx.send(AttachJob::Detach {
                                    pipeline: pipeline.clone(),
                                    concat: concat.clone(),
                                    bin: stale.bin,
                                    branch_queue: stale.branch_queue,
                                });
                            }
                        }
                    }

                    AudioCommand::ListDevices { reply } => {
                        let result = list_alsa_devices_inner();
                        reply.send(result).ok();
                    }
                }
            }
        });

        Self {
            cmd_tx,
            output_state,
            output_transaction,
            exclusive_device,
            decoded_caps_cell,
            output_caps_cell,
        }
    }

    fn send_cmd<T>(&self, build: impl FnOnce(Reply<T>) -> AudioCommand) -> T {
        let (tx, rx) = mpsc::channel();
        let cmd = build(tx);
        self.cmd_tx.send(cmd).expect("Audio thread dead");
        rx.recv().expect("Audio thread dead")
    }

    pub fn play_url(
        &self,
        uri: &str,
        start_secs: Option<f32>,
        track_id: u64,
    ) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::PlayUrl {
            uri: uri.to_string(),
            start_secs,
            preserve_output: false,
            track_id: Some(track_id),
            resume_paused: false,
            reply,
        })
    }
    pub fn pause(&self) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::Pause { reply })
    }
    pub fn resume(&self) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::Resume { reply })
    }
    pub fn stop(&self) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::Stop { reply })
    }
    pub fn set_volume(&self, level: f32) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::SetVolume { level, reply })
    }
    pub fn set_normalization_gain(&self, gain: f64) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::SetNormalizationGain { gain, reply })
    }
    pub fn seek(&self, position_secs: f32) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::Seek {
            position_secs,
            reply,
        })
    }
    pub fn get_position(&self) -> Result<f32, String> {
        self.send_cmd(|reply| AudioCommand::GetPosition { reply })
    }
    pub fn is_finished(&self) -> Result<bool, String> {
        self.send_cmd(|reply| AudioCommand::IsFinished { reply })
    }
    /// Hold across the settings transaction; PlayUrl only takes this gate to
    /// snapshot a committed route, then releases it before any audio/network IO.
    pub fn begin_output_update(&self) -> std::sync::MutexGuard<'_, ()> {
        self.output_transaction.lock().unwrap()
    }

    pub fn configure_output(&self, config: AudioOutputConfig) -> Result<(), String> {
        let mut state = self.output_state.lock().unwrap();
        let changed = !state.configured.same_processing_as(&config);
        let previous = state.clone();
        let active = state.active.clone();
        replace_output_state(&mut state, config, active);
        if changed && self.cmd_tx.send(AudioCommand::OutputChanged).is_err() {
            replace_output_state(&mut state, previous.configured, previous.active);
            return Err("Audio worker is unavailable".into());
        }
        Ok(())
    }

    pub fn output_state(&self) -> AudioOutputState {
        let _transaction = self.output_transaction.lock().unwrap();
        self.output_state.lock().unwrap().clone()
    }

    pub fn set_exclusive_mode(&self, enabled: bool, device: Option<String>) -> Result<(), String> {
        let mut config = self.output_state().configured;
        config.exclusive_mode = enabled;
        config.device = device;
        if !enabled {
            config.bit_perfect = false;
        }
        self.configure_output(config)
    }

    pub fn set_bit_perfect(&self, enabled: bool) -> Result<(), String> {
        let mut config = self.output_state().configured;
        config.bit_perfect = enabled;
        if enabled {
            config.exclusive_mode = true;
        }
        self.configure_output(config)
    }

    pub fn set_proxy_settings(&self, settings: crate::ProxySettings) {
        self.send_cmd(|reply| AudioCommand::SetProxySettings { settings, reply });
    }
    pub fn set_gapless(&self, enabled: bool) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::SetGapless { enabled, reply })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn set_next_track(
        &self,
        uri: String,
        norm_gain: f64,
        track_id: u64,
        qid: String,
        replay_gain: f64,
        peak_amplitude: f64,
        is_dash: bool,
    ) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::SetNextTrack {
            uri,
            norm_gain,
            track_id,
            qid,
            replay_gain,
            peak_amplitude,
            is_dash,
            reply,
        })
    }
    pub fn clear_next_track(&self) -> Result<(), String> {
        self.send_cmd(|reply| AudioCommand::ClearNextTrack { reply })
    }
    pub fn list_devices(&self) -> Result<Vec<AudioDevice>, String> {
        self.send_cmd(|reply| AudioCommand::ListDevices { reply })
    }

    pub fn snapshot_decoded_caps(&self) -> Option<crate::pipeline_probe::PadCaps> {
        self.decoded_caps_cell.lock().ok()?.clone()
    }

    pub fn snapshot_output_caps(&self) -> Option<crate::pipeline_probe::PadCaps> {
        self.output_caps_cell.lock().ok()?.clone()
    }

    /// Returns the ALSA device string for DirectAlsa, or None for Normal mode.
    pub fn exclusive_device(&self) -> Option<String> {
        self.exclusive_device.lock().ok()?.clone()
    }
}

fn release_alsa_writer(
    writer_tx: &mut Option<crossbeam_channel::Sender<WriterCommand>>,
    writer_thread: &mut Option<JoinHandle<()>>,
    writer_fmt: &mut Option<PcmFormat>,
    writer_supported_fmts: &mut Option<Vec<&'static str>>,
    writer_supported_rates: &mut Option<Vec<u32>>,
    writer_device: &mut Option<String>,
    writer_bit_perfect: &mut Option<bool>,
) {
    if let Some(tx) = writer_tx.take() {
        tx.try_send(WriterCommand::Shutdown).ok();
    }
    if let Some(handle) = writer_thread.take() {
        handle.join().ok();
    }
    *writer_fmt = None;
    *writer_supported_fmts = None;
    *writer_supported_rates = None;
    *writer_device = None;
    *writer_bit_perfect = None;
}

fn hq_send_transport(
    control: &Mutex<Option<crate::hqplayer::ControlSession>>,
    host: &str,
    port: u16,
    op: impl FnMut(&mut crate::hqplayer::ControlSession) -> Result<(), crate::hqplayer::ControlError>,
) -> Result<(), String> {
    let mut slot = control.lock().unwrap_or_else(|p| p.into_inner());
    crate::hqplayer::call(&mut slot, host, port, op).map_err(|e| e.to_string())
}

fn hq_send_stop(
    control: &Mutex<Option<crate::hqplayer::ControlSession>>,
    host: &str,
    port: u16,
) -> Result<(), String> {
    hq_send_transport(control, host, port, |session| session.stop())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = {
            let mut slot = control.lock().unwrap_or_else(|p| p.into_inner());
            crate::hqplayer::retry_read(&mut slot, host, port, |session| session.status())
                .map_err(|e| e.to_string())?
        };
        if matches!(status.state, 0 | 3) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("HQPlayer did not confirm stop; local output remains closed".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn playback_config(state: &AudioOutputState, preserve_output: bool) -> AudioOutputConfig {
    if preserve_output {
        state
            .active
            .clone()
            .unwrap_or_else(|| state.configured.effective())
    } else {
        state.configured.effective()
    }
}

fn replace_output_state(
    state: &mut AudioOutputState,
    configured: AudioOutputConfig,
    active: Option<AudioOutputConfig>,
) {
    let revision = state.revision.saturating_add(1);
    let playback_generation = state.playback_generation.filter(|_| active.is_some());
    *state = AudioOutputState::new(configured, active);
    state.revision = revision;
    state.playback_generation = playback_generation;
}

static NEXT_PLAYBACK_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_playback_generation() -> u64 {
    NEXT_PLAYBACK_GENERATION.fetch_add(1, Ordering::Relaxed)
}

fn bump_hq_generation(transaction: &Mutex<()>, generation: &AtomicU64) -> u64 {
    let _transaction = transaction.lock().unwrap();
    generation.fetch_add(1, Ordering::AcqRel) + 1
}

fn with_current_generation(
    transaction: &Mutex<()>,
    generation: (&AtomicU64, u64),
    apply: impl FnOnce(),
) -> bool {
    let _transaction = transaction.lock().unwrap();
    if generation.0.load(Ordering::Acquire) != generation.1 {
        return false;
    }
    apply();
    true
}

fn publish_output_state(
    app: &tauri::AppHandle,
    state: &Mutex<AudioOutputState>,
    transaction: &Mutex<()>,
    active: Option<AudioOutputConfig>,
) {
    let _transaction = transaction.lock().unwrap();
    let snapshot = {
        let mut state = state.lock().unwrap();
        let configured = state.configured.clone();
        let playback_generation = active.as_ref().map(|_| next_playback_generation());
        replace_output_state(&mut state, configured, active);
        state.playback_generation = playback_generation;
        state.clone()
    };
    let _ = app.emit("audio-output-changed", snapshot);
}

fn publish_hq_output(
    app: &tauri::AppHandle,
    watch: &HqWatch,
    active: Option<AudioOutputConfig>,
    generation: (&AtomicU64, u64),
    playback_generation: u64,
) -> bool {
    with_current_generation(&watch.output_transaction, generation, || {
        let snapshot = {
            let mut state = watch.output_state.lock().unwrap();
            // A finished old gapless arm must not clear its successor's route.
            if active.is_none()
                && state
                    .playback_generation
                    .is_some_and(|token| token != playback_generation)
            {
                return;
            }
            let configured = state.configured.clone();
            let token = active.as_ref().map(|_| playback_generation);
            replace_output_state(&mut state, configured, active);
            state.playback_generation = token;
            state.clone()
        };
        let _ = app.emit("audio-output-changed", snapshot);
    })
}

fn emit_hq_buffering(arm: &HqArm, buffering: bool) -> bool {
    let mut emitted = false;
    with_current_generation(
        &arm.watch.output_transaction,
        (&arm.gen_cell, arm.gen),
        || {
            if arm.feed.is_cancelled() {
                return;
            }
            let current = arm.watch.output_state.lock().unwrap().playback_generation;
            if current != Some(arm.playback_generation) {
                return;
            }
            if let Some(track_id) = arm.track_id {
                let _ = arm.app.emit(
                    "audio-buffering",
                    serde_json::json!({
                        "buffering": buffering, "trackId": track_id,
                        "playbackGeneration": arm.playback_generation,
                    }),
                );
                emitted = true;
            }
        },
    );
    emitted
}

/// A decoded next track waiting for HQPlayer to cross into it.
struct HqPrepared {
    playback_generation: u64,
    pipeline: gst::Pipeline,
    feed: crate::hqplayer::WavFeed,
    heard: Arc<AtomicU32>,
    finish_emit: Arc<AtomicBool>,
    uri: String,
    track_id: u64,
    qid: String,
    norm_gain: f64,
    replay_gain: f64,
    peak_amplitude: f64,
    rate: u32,
    channels: u32,
    /// `PlayNextURI` has been sent. Until then the row is not in Desktop's playlist.
    sent: bool,
}

#[derive(Clone)]
struct HqQueueMeta {
    token: u64,
    uri: String,
    track_id: u64,
    qid: String,
    norm_gain: f64,
    replay_gain: f64,
    peak_amplitude: f64,
}

/// Shared with the arm threads and the status watcher.
struct HqPreparing {
    token: u64,
    track_id: u64,
    qid: String,
    feed: crate::hqplayer::WavFeed,
    pipeline: gst::Pipeline,
}

struct HqWatch {
    cmd_tx: mpsc::Sender<AudioCommand>,
    next: Mutex<Option<HqPrepared>>,
    preparing: Mutex<Option<HqPreparing>>,
    last_status: Mutex<Option<crate::hqplayer::Status>>,
    output_state: Arc<Mutex<AudioOutputState>>,
    output_transaction: Arc<Mutex<()>>,
    /// `hq_gen` of the track whose watcher has observed play. Zero before that.
    started_gen: AtomicU64,
    advancing: AtomicBool,
    next_gen: AtomicU64,
}

fn hq_crossed_boundary(
    previous: crate::hqplayer::Status,
    current: crate::hqplayer::Status,
) -> bool {
    current.state == 2
        && previous.length > 1.0
        && previous.position + 5.0 >= previous.length
        && current.position < 2.0
        && previous.position > current.position + 5.0
}

fn cancel_hq_for_output_change(
    watch: &HqWatch,
    control: &Mutex<Option<crate::hqplayer::ControlSession>>,
    host: &str,
    port: u16,
) {
    if watch.advancing.load(Ordering::Acquire) {
        return;
    }
    let prepared = watch
        .next
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|item| item.sent);
    if prepared {
        // A control snapshot closes the window between Desktop switching and
        // our periodic watcher observing it. Never remove a WAV already heard.
        let previous = *watch.last_status.lock().unwrap();
        let current = {
            let mut session = control.lock().unwrap_or_else(|p| p.into_inner());
            crate::hqplayer::retry_read(&mut session, host, port, |session| session.status())
        };
        if let (Some(previous), Ok(current)) = (previous, current) {
            if hq_crossed_boundary(previous, current) {
                if !watch.advancing.swap(true, Ordering::AcqRel) {
                    let _ = watch.cmd_tx.send(AudioCommand::HandleHqAdvance);
                }
                return;
            }
        }
    }
    cancel_hq_prepared(watch, control, host, port);
}

fn cancel_hq_prepared(
    watch: &HqWatch,
    control: &Mutex<Option<crate::hqplayer::ControlSession>>,
    host: &str,
    port: u16,
) {
    watch.next_gen.fetch_add(1, Ordering::AcqRel);
    if let Some(preparing) = watch.preparing.lock().unwrap().take() {
        preparing.feed.cancel();
        if let Some(bus) = preparing.pipeline.bus() {
            bus.set_flushing(true);
        }
        let _ = preparing.pipeline.set_state(gst::State::Null);
    }
    let prepared = match watch.next.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    let Some(prepared) = prepared else {
        return;
    };
    if prepared.sent {
        let _ = hq_send_transport(control, host, port, |session| session.playlist_remove(1));
    }
    prepared.finish_emit.store(false, Ordering::SeqCst);
    prepared.feed.cancel();
    let _ = prepared.pipeline.set_state(gst::State::Null);
}

struct HqLaunch {
    playback_generation: u64,
    started: mpsc::Receiver<Result<(), String>>,
    pipeline: gst::Pipeline,
    feed: crate::hqplayer::WavFeed,
    heard: Arc<AtomicU32>,
    finish_emit: Arc<AtomicBool>,
}

type HqStartupReply = Arc<Mutex<Option<Reply<Result<(), String>>>>>;

struct HqArm {
    playback_generation: u64,
    startup: HqStartupReply,
    resume_paused: bool,
    track_id: Option<u64>,
    paused: Arc<AtomicBool>,
    pipeline: gst::Pipeline,
    feed: crate::hqplayer::WavFeed,
    control: Arc<Mutex<Option<crate::hqplayer::ControlSession>>>,
    gen: u64,
    gen_cell: Arc<AtomicU64>,
    origin: f32,
    heard: Arc<AtomicU32>,
    finish_emit: Arc<AtomicBool>,
    eos: Arc<AtomicBool>,
    reported: Arc<AtomicBool>,
    app: tauri::AppHandle,
    signal: Arc<SignalPathTracker>,
    decoded_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    output_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    host: String,
    port: u16,
    tearing_down: Arc<AtomicBool>,
    watch: Arc<HqWatch>,
    queue: Option<HqQueueMeta>,
}

#[allow(clippy::too_many_arguments)]
fn launch_hqplayer_track(
    uri: &str,
    is_dash: bool,
    route: crate::proxy::Route,
    origin: f32,
    gen: u64,
    gen_cell: Arc<AtomicU64>,
    control: Arc<Mutex<Option<crate::hqplayer::ControlSession>>>,
    host: String,
    port: u16,
    app: tauri::AppHandle,
    tearing_down: Arc<AtomicBool>,
    eos: Arc<AtomicBool>,
    signal: Arc<SignalPathTracker>,
    decoded_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    output_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    watch: Arc<HqWatch>,
    queue: Option<HqQueueMeta>,
    resume_paused: bool,
    track_id: Option<u64>,
    paused: Arc<AtomicBool>,
) -> Result<HqLaunch, String> {
    let feed = crate::hqplayer::WavFeed::bind()?;
    let pipeline = match build_hqplayer_pipeline(uri, is_dash, route, &feed) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            feed.cancel();
            return Err(err);
        }
    };
    if let Some(meta) = queue.as_ref() {
        let mut preparing = watch.preparing.lock().unwrap();
        if watch.next_gen.load(Ordering::Acquire) != meta.token {
            feed.cancel();
            let _ = pipeline.set_state(gst::State::Null);
            return Err("Preload was superseded".into());
        }
        *preparing = Some(HqPreparing {
            token: meta.token,
            track_id: meta.track_id,
            qid: meta.qid.clone(),
            feed: feed.clone(),
            pipeline: pipeline.clone(),
        });
    }
    let heard = Arc::new(AtomicU32::new(origin.to_bits()));
    let finish_emit = Arc::new(AtomicBool::new(true));
    let (started_tx, started) = mpsc::channel();
    let playback_generation = next_playback_generation();
    let arm = HqArm {
        playback_generation,
        startup: Arc::new(Mutex::new(Some(started_tx))),
        resume_paused,
        track_id,
        paused,
        pipeline: pipeline.clone(),
        feed: feed.clone(),
        control,
        gen,
        gen_cell,
        origin,
        heard: Arc::clone(&heard),
        finish_emit: Arc::clone(&finish_emit),
        eos,
        reported: Arc::new(AtomicBool::new(false)),
        app,
        signal,
        decoded_cell,
        output_cell,
        host,
        port,
        tearing_down,
        watch,
        queue,
    };
    if let Err(err) = std::thread::Builder::new()
        .name("hqplayer-arm".into())
        .spawn(move || run_hqplayer_arm(arm))
    {
        feed.cancel();
        let _ = pipeline.set_state(gst::State::Null);
        return Err(format!("Failed to start HQPlayer handoff: {err}"));
    }
    Ok(HqLaunch {
        playback_generation,
        started,
        pipeline,
        feed,
        heard,
        finish_emit,
    })
}

fn build_hqplayer_pipeline(
    uri: &str,
    is_dash: bool,
    route: crate::proxy::Route,
    feed: &crate::hqplayer::WavFeed,
) -> Result<gst::Pipeline, String> {
    use gst_app::prelude::*;

    let pipe = gst::Pipeline::new();
    let built = (|| -> Result<(), String> {
        watch_pipeline_sources(&pipe, route);
        let mut udb = gst::ElementFactory::make("uridecodebin").property("uri", uri);
        if is_dash {
            udb = udb
                .property("buffer-duration", 15_000_000_000i64)
                .property("use-buffering", true);
        } else {
            udb = udb
                .property("buffer-duration", 5_000_000_000i64)
                .property("use-buffering", true);
        }
        let uridecodebin = udb
            .build()
            .map_err(|err| format!("Failed to create uridecodebin: {err}"))?;
        let audioconvert = gst::ElementFactory::make("audioconvert")
            .property_from_str("dithering", "none")
            .property_from_str("noise-shaping", "none")
            .build()
            .map_err(|err| format!("Failed to create audioconvert: {err}"))?;
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S32LE")
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &caps)
            .build()
            .map_err(|err| format!("Failed to create capsfilter: {err}"))?;
        let appsink = gst_app::AppSink::builder()
            .sync(false)
            .drop(false)
            .max_buffers(64)
            .build();

        let feed_samples = feed.clone();
        let feed_eos = feed.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if feed_samples.is_cancelled() {
                        return Err(gst::FlowError::Eos);
                    }
                    let token = feed_samples.capture_token();
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let format = parse_pcm_format(caps).ok_or(gst::FlowError::Error)?;
                    if format.gst_format != "S32LE" || format.bytes_per_sample != 4 {
                        return Err(gst::FlowError::Error);
                    }
                    feed_samples.set_format(format.sample_rate, format.channels);
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    feed_samples.offer(token, map.as_slice());
                    if feed_samples.failure().is_some() {
                        return Err(gst::FlowError::Error);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .eos(move |_sink| {
                    feed_eos.finish();
                })
                .build(),
        );

        pipe.add_many([
            &uridecodebin,
            &audioconvert,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .map_err(|err| format!("Failed to add elements: {err}"))?;
        gst::Element::link_many([&audioconvert, &capsfilter, appsink.upcast_ref()])
            .map_err(|err| format!("Failed to link HQPlayer chain: {err}"))?;

        let convert_weak = audioconvert.downgrade();
        uridecodebin.connect_pad_added(move |_src, src_pad| {
            if let Some(caps) = src_pad.current_caps() {
                if let Some(structure) = caps.structure(0) {
                    if !structure.name().as_str().starts_with("audio/") {
                        return;
                    }
                }
            }
            let Some(convert) = convert_weak.upgrade() else {
                return;
            };
            let Some(sink_pad) = convert.static_pad("sink") else {
                return;
            };
            if sink_pad.is_linked() {
                return;
            }
            if let Err(err) = src_pad.link(&sink_pad) {
                log::warn!("[hqplayer] pad link failed: {err:?}");
            }
        });
        Ok(())
    })();
    if let Err(err) = built {
        let _ = pipe.set_state(gst::State::Null);
        return Err(err);
    }
    Ok(pipe)
}

fn hq_superseded(arm: &HqArm) -> bool {
    arm.feed.is_cancelled()
        || arm.gen_cell.load(Ordering::Acquire) != arm.gen
        || arm
            .queue
            .as_ref()
            .is_some_and(|meta| arm.watch.next_gen.load(Ordering::Acquire) != meta.token)
}

fn hq_emit_error(arm: &HqArm, message: &str) {
    let _transaction = arm.watch.output_transaction.lock().unwrap();
    let active_generation = arm.watch.output_state.lock().unwrap().playback_generation;
    if active_generation.is_some_and(|token| token != arm.playback_generation) {
        return;
    }
    log::error!("[hqplayer] {message}");
    if arm.tearing_down.load(Ordering::SeqCst) {
        return;
    }
    if arm.gen_cell.load(Ordering::Acquire) != arm.gen {
        return;
    }
    if arm
        .reported
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let _ = arm.app.emit(
        "audio-error",
        serde_json::json!({
            "kind": "playback_error",
            "message": message,
        }),
    );
}

fn hq_fail(arm: &HqArm, message: &str) {
    hq_start_result(arm, Err(message.into()));
    arm.feed.cancel();
    let _ = arm.pipeline.set_state(gst::State::Null);
    if arm.queue.is_some() {
        if message != "cancelled" {
            log::warn!("[hqplayer] next track not queued: {message}");
        }
        return;
    }
    if arm.gen_cell.load(Ordering::Acquire) != arm.gen {
        return;
    }
    publish_hq_output(
        &arm.app,
        &arm.watch,
        None,
        (&arm.gen_cell, arm.gen),
        arm.playback_generation,
    );
    hq_emit_error(arm, message);
}

fn wait_until_paused(
    pipeline: &gst::Pipeline,
    budget: std::time::Duration,
    mut stale: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if stale() {
            return Err("cancelled".into());
        }
        let (ret, cur, _) = pipeline.state(gst::ClockTime::from_mseconds(200));
        if let Err(err) = ret {
            return Err(format!("decoder failed: {err}"));
        }
        if cur >= gst::State::Paused {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for the decoder".into());
        }
    }
}

fn hq_wav_len(arm: &HqArm, rate: u32, channels: u32) -> Result<Option<u32>, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if hq_superseded(arm) {
            return Err("cancelled".into());
        }
        if let Some(duration) = arm.pipeline.query_duration::<gst::ClockTime>() {
            if duration.nseconds() > 0 {
                let dur_secs = duration.nseconds() as f64 / 1_000_000_000.0;
                let remain = (dur_secs - f64::from(arm.origin)).max(0.0);
                return match crate::hqplayer::bytes_from_duration(remain, rate, channels) {
                    Some(0) => Err("HQPlayer seek is past the end of the track".into()),
                    Some(bytes) => Ok(Some(bytes)),
                    None => Err("track is too long for a WAV handoff".into()),
                };
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn hq_handoff(arm: &HqArm) -> Result<(), crate::hqplayer::ControlError> {
    let url = arm.feed.url();
    let gen = arm.gen;
    let gen_cell = Arc::clone(&arm.gen_cell);
    let feed = arm.feed.clone();
    let host = arm.host.clone();
    let port = arm.port;
    log::info!("[hqplayer] handoff {url}");
    let mut slot = arm
        .control
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    crate::hqplayer::call(&mut slot, &host, port, |session| {
        if gen_cell.load(Ordering::Acquire) != gen || feed.is_cancelled() {
            return Ok(());
        }
        if let Err(err) = session.stop() {
            if matches!(err, crate::hqplayer::ControlError::Transport(_)) {
                return Err(err);
            }
            log::warn!("[hqplayer] stop before handoff: {err}");
        }
        if gen_cell.load(Ordering::Acquire) != gen || feed.is_cancelled() {
            return Ok(());
        }
        session.playlist_clear()?;
        if gen_cell.load(Ordering::Acquire) != gen || feed.is_cancelled() {
            return Ok(());
        }
        session.play_next_uri(&url)?;
        if gen_cell.load(Ordering::Acquire) != gen || feed.is_cancelled() {
            let _ = session.stop();
            return Ok(());
        }
        if let Err(err) = session.play() {
            if matches!(err, crate::hqplayer::ControlError::Transport(_)) {
                return Err(err);
            }
            log::warn!("[hqplayer] play after PlayNextURI: {err}");
        }
        Ok(())
    })
}

fn spawn_hq_bus(arm: &HqArm) {
    let Some(bus) = arm.pipeline.bus() else {
        return;
    };
    let feed = arm.feed.clone();
    let reported = Arc::clone(&arm.reported);
    let app = arm.app.clone();
    let tearing = Arc::clone(&arm.tearing_down);
    let gen = arm.gen;
    let gen_cell = Arc::clone(&arm.gen_cell);
    let report_errors = arm.queue.is_none();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            if gen_cell.load(Ordering::Acquire) != gen || feed.is_cancelled() {
                break;
            }
            match msg.view() {
                gst::MessageView::Eos(..) => {
                    feed.finish();
                    break;
                }
                gst::MessageView::Error(err) => {
                    let err_msg = err.error().to_string();
                    log::error!("[hqplayer] gstreamer: {err_msg}");
                    feed.cancel();
                    if report_errors
                        && !tearing.load(Ordering::SeqCst)
                        && gen_cell.load(Ordering::Acquire) == gen
                        && reported
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        let _ = app.emit(
                            "audio-error",
                            serde_json::json!({
                                "kind": "playback_error",
                                "message": err_msg,
                            }),
                        );
                    }
                    break;
                }
                _ => {}
            }
        }
    });
}

fn run_hqplayer_arm(arm: HqArm) {
    spawn_hq_bus(&arm);
    if let Err(err) = arm.pipeline.set_state(gst::State::Paused) {
        hq_fail(&arm, &format!("Failed to pause the decoder: {err}"));
        return;
    }
    let paused_budget = std::time::Duration::from_secs(20);
    let feed = arm.feed.clone();
    let gen = arm.gen;
    let gen_cell = Arc::clone(&arm.gen_cell);
    if let Err(err) = wait_until_paused(&arm.pipeline, paused_budget, move || {
        feed.is_cancelled() || gen_cell.load(Ordering::Acquire) != gen
    }) {
        if err != "cancelled" {
            hq_fail(&arm, &err);
        }
        return;
    }
    if hq_superseded(&arm) {
        return;
    }
    if arm.origin > 0.5 {
        arm.feed.drop_early();
        let pos = gst::ClockTime::from_nseconds((f64::from(arm.origin) * 1_000_000_000.0) as u64);
        if let Err(err) = arm
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, pos)
        {
            hq_fail(&arm, &format!("HQPlayer seek failed: {err}"));
            return;
        }
        let feed = arm.feed.clone();
        let gen = arm.gen;
        let gen_cell = Arc::clone(&arm.gen_cell);
        if let Err(err) = wait_until_paused(&arm.pipeline, paused_budget, move || {
            feed.is_cancelled() || gen_cell.load(Ordering::Acquire) != gen
        }) {
            if err != "cancelled" {
                hq_fail(&arm, &err);
            }
            return;
        }
        if hq_superseded(&arm) {
            return;
        }
    }
    arm.feed.begin_capture();
    if let Err(err) = arm.pipeline.set_state(gst::State::Playing) {
        hq_fail(&arm, &format!("Failed to start the decoder: {err}"));
        return;
    }
    let (rate, channels) = match arm.feed.wait_format(std::time::Duration::from_secs(15)) {
        Ok(format) => format,
        Err(err) => {
            if err == "cancelled" || hq_superseded(&arm) {
                return;
            }
            hq_fail(&arm, &err);
            return;
        }
    };
    if hq_superseded(&arm) {
        return;
    }
    if arm.queue.is_none() {
        let caps = crate::pipeline_probe::PadCaps {
            format: "S32LE".to_string(),
            rate,
            channels,
        };
        if let Ok(mut guard) = arm.decoded_cell.lock() {
            *guard = Some(caps.clone());
        }
        if let Ok(mut guard) = arm.output_cell.lock() {
            *guard = Some(caps);
        }
        arm.signal.set_decoded("S32LE", rate, channels);
        arm.signal.set_output("S32LE", rate, channels);
    }

    match hq_wav_len(&arm, rate, channels) {
        Ok(Some(bytes)) => {
            if let Err(err) = arm.feed.set_data_bytes(bytes) {
                if err == "cancelled" || hq_superseded(&arm) {
                    return;
                }
                hq_fail(&arm, &err);
                return;
            }
        }
        Ok(None) => {
            if let Err(err) = arm.feed.wait_finished(std::time::Duration::from_secs(30)) {
                if err == "cancelled" || hq_superseded(&arm) {
                    return;
                }
                hq_fail(&arm, &err);
                return;
            }
        }
        Err(err) => {
            if err == "cancelled" || hq_superseded(&arm) {
                return;
            }
            hq_fail(&arm, &err);
            return;
        }
    }
    if hq_superseded(&arm) {
        return;
    }
    if arm.queue.is_some() {
        let meta = arm.queue.clone().expect("queue meta");
        if let Err(err) = queue_hq_uri(&arm, &meta, rate, channels) {
            if err != "cancelled" {
                log::warn!("[hqplayer] next track not queued: {err}");
            }
            arm.feed.cancel();
            let _ = arm.pipeline.set_state(gst::State::Null);
        }
        return;
    }
    if let Err(err) = hq_handoff(&arm) {
        hq_fail(&arm, &err.to_string());
        return;
    }
    if hq_superseded(&arm) {
        return;
    }
    spawn_hq_watcher(arm, false);
}

fn spawn_hq_watcher(arm: HqArm, assume_started: bool) {
    if let Err(err) = std::thread::Builder::new()
        .name("hqplayer-watch".into())
        .spawn(move || run_hq_watcher(arm, assume_started))
    {
        log::error!("[hqplayer] watcher failed to start: {err}");
    }
}

fn queue_hq_uri(arm: &HqArm, meta: &HqQueueMeta, rate: u32, channels: u32) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if hq_superseded(arm) || arm.watch.next_gen.load(Ordering::Acquire) != meta.token {
            return Err("cancelled".into());
        }
        if arm.watch.started_gen.load(Ordering::Acquire) == arm.gen {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err("current track did not start".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let url = arm.feed.url();
    let host = arm.host.clone();
    let port = arm.port;
    let gen = arm.gen;
    let gen_cell = Arc::clone(&arm.gen_cell);
    let token = meta.token;
    let next_gen = Arc::clone(&arm.watch);
    let sent = {
        let mut slot = arm
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::hqplayer::call(&mut slot, &host, port, |session| {
            if gen_cell.load(Ordering::Acquire) != gen
                || next_gen.next_gen.load(Ordering::Acquire) != token
            {
                return Ok(false);
            }
            session.play_next_uri(&url)?;
            Ok(true)
        })
        .map_err(|err| err.to_string())?
    };
    // `sent` is false when the closure returned before PlayNextURI. Removing
    // index 1 then would drop a row this arm did not add.
    if !sent {
        return Err("cancelled".into());
    }
    if hq_superseded(arm) || arm.watch.next_gen.load(Ordering::Acquire) != meta.token {
        let _ = hq_send_transport(&arm.control, &arm.host, arm.port, |session| {
            session.playlist_remove(1)
        });
        return Err("cancelled".into());
    }
    let prepared = HqPrepared {
        playback_generation: arm.playback_generation,
        pipeline: arm.pipeline.clone(),
        feed: arm.feed.clone(),
        heard: Arc::clone(&arm.heard),
        finish_emit: Arc::clone(&arm.finish_emit),
        uri: meta.uri.clone(),
        track_id: meta.track_id,
        qid: meta.qid.clone(),
        norm_gain: meta.norm_gain,
        replay_gain: meta.replay_gain,
        peak_amplitude: meta.peak_amplitude,
        rate,
        channels,
        sent: true,
    };
    let mut guard = arm
        .watch
        .next
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if arm.watch.next_gen.load(Ordering::Acquire) != meta.token {
        drop(guard);
        let _ = hq_send_transport(&arm.control, &arm.host, arm.port, |session| {
            session.playlist_remove(1)
        });
        return Err("cancelled".into());
    }
    *guard = Some(prepared);
    drop(guard);
    let mut preparing = arm.watch.preparing.lock().unwrap();
    if preparing
        .as_ref()
        .is_some_and(|item| item.token == meta.token)
    {
        preparing.take();
    }
    Ok(())
}

enum HqPoll {
    Status(crate::hqplayer::Status),
    Transport,
    Rejected,
    Superseded,
}

fn hq_poll_status(arm: &HqArm) -> HqPoll {
    let host = arm.host.clone();
    let port = arm.port;
    let gen = arm.gen;
    let gen_cell = Arc::clone(&arm.gen_cell);
    let mut slot = arm
        .control
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match crate::hqplayer::retry_read(&mut slot, &host, port, |session| {
        if gen_cell.load(Ordering::Acquire) != gen {
            return Ok(None);
        }
        session.status().map(Some)
    }) {
        Ok(Some(status)) => HqPoll::Status(status),
        Ok(None) => HqPoll::Superseded,
        Err(crate::hqplayer::ControlError::Transport(_)) => HqPoll::Transport,
        Err(err) => {
            log::warn!("[hqplayer] status: {err}");
            HqPoll::Rejected
        }
    }
}

fn hq_did_not_start(arm: &HqArm) {
    hq_start_result(arm, Err("HQPlayer did not start playback".into()));
    arm.feed.cancel();
    let _ = arm.pipeline.set_state(gst::State::Null);
    hq_emit_error(
        arm,
        "HQPlayer did not start playback. It needs the DAC free and the settings dialog closed.",
    );
    arm.finish_emit.store(false, Ordering::SeqCst);
}

fn hq_start_result(arm: &HqArm, result: Result<(), String>) {
    if let Some(reply) = arm.startup.lock().unwrap().take() {
        let _ = reply.send(result);
    }
}

fn hq_mark_started(arm: &HqArm) {
    if arm.gen_cell.load(Ordering::Acquire) == arm.gen {
        if arm.resume_paused && arm.startup.lock().unwrap().is_some() {
            if let Err(error) =
                hq_send_transport(&arm.control, &arm.host, arm.port, |session| session.pause())
            {
                hq_fail(arm, &error);
                return;
            }
        }
        hq_start_result(arm, Ok(()));
        arm.watch.started_gen.store(arm.gen, Ordering::Release);
    }
}

fn hq_next_is_sent(arm: &HqArm) -> bool {
    let guard = arm
        .watch
        .next
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().is_some_and(|prepared| prepared.sent)
}

fn run_hq_watcher(arm: HqArm, assume_started: bool) {
    if !arm.finish_emit.load(Ordering::SeqCst) {
        return;
    }
    let started_at = std::time::Instant::now();
    let mut started = assume_started;
    if assume_started {
        hq_mark_started(&arm);
    }
    let mut seen_position = 0.0f64;
    let mut seen_length = 0.0f64;
    let mut stopped_polls = 0u8;
    let mut idle_since = std::time::Instant::now();
    let mut transport_since: Option<std::time::Instant> = None;
    let mut buffering = false;
    loop {
        if !arm.finish_emit.load(Ordering::SeqCst)
            || arm.feed.is_cancelled()
            || arm.watch.advancing.load(Ordering::Acquire)
        {
            return;
        }
        if arm.gen_cell.load(Ordering::Acquire) != arm.gen {
            return;
        }
        if !started && started_at.elapsed() >= std::time::Duration::from_secs(20) {
            hq_did_not_start(&arm);
            return;
        }
        if let Some(error) = arm.feed.failure() {
            hq_fail(&arm, &error);
            return;
        }
        let readers = arm.feed.reader_count();
        if readers > 0 {
            idle_since = std::time::Instant::now();
        }
        match hq_poll_status(&arm) {
            HqPoll::Superseded => return,
            HqPoll::Rejected | HqPoll::Transport => {
                if !buffering && started {
                    buffering = emit_hq_buffering(&arm, true);
                }
                let since = transport_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= std::time::Duration::from_secs(12) {
                    arm.feed.cancel();
                    let _ = arm.pipeline.set_state(gst::State::Null);
                    hq_fail(&arm, "HQPlayer control connection closed");
                    arm.finish_emit.store(false, Ordering::SeqCst);
                    return;
                }
            }
            HqPoll::Status(status) => {
                if hq_superseded(&arm) {
                    return;
                }
                if status.state == 2 && arm.paused.load(Ordering::Acquire) {
                    if let Err(error) =
                        hq_send_transport(&arm.control, &arm.host, arm.port, |session| {
                            session.pause()
                        })
                    {
                        hq_fail(&arm, &error);
                        return;
                    }
                }
                if buffering && status.state == 2 && !arm.paused.load(Ordering::Acquire) {
                    emit_hq_buffering(&arm, false);
                    buffering = false;
                }
                transport_since = None;
                let pos = arm.origin + status.position as f32;
                arm.heard.store(pos.to_bits(), Ordering::Relaxed);
                match status.state {
                    2 => {
                        hq_mark_started(&arm);
                        stopped_polls = 0;
                        idle_since = std::time::Instant::now();
                        // The previous sample is the end of the track that was
                        // playing. A restart is the queued WAV taking over
                        // while Desktop stays in play. Length is not required
                        // to change: two tracks can share a duration.
                        let crossed = hq_crossed_boundary(
                            crate::hqplayer::Status {
                                state: 2,
                                position: seen_position,
                                length: seen_length,
                            },
                            status,
                        );
                        let ready = hq_next_is_sent(&arm);
                        if started
                            && crossed
                            && ready
                            && arm.finish_emit.swap(false, Ordering::SeqCst)
                            && arm.gen_cell.load(Ordering::Acquire) == arm.gen
                        {
                            if arm.watch.advancing.swap(true, Ordering::AcqRel) {
                                return;
                            }
                            if arm
                                .watch
                                .cmd_tx
                                .send(AudioCommand::HandleHqAdvance)
                                .is_err()
                            {
                                arm.watch.advancing.store(false, Ordering::Release);
                            }
                            return;
                        }
                        started = true;
                        if status.length > 0.0 {
                            seen_length = status.length;
                            seen_position = status.position;
                        }
                        *arm.watch.last_status.lock().unwrap() = Some(status);
                    }
                    1 => {
                        stopped_polls = 0;
                        idle_since = std::time::Instant::now();
                    }
                    0 | 3 => {
                        if started {
                            // A queued next that was already at the end is the
                            // gapless boundary, not an early stop. Two stopped
                            // polls still fall back to track-finished.
                            let queued_near_end = hq_next_is_sent(&arm)
                                && seen_length > 1.0
                                && seen_position + 5.0 >= seen_length;
                            let near_end = queued_near_end
                                || (status.length > 0.0 && status.position + 1.5 >= status.length);
                            stopped_polls = stopped_polls.saturating_add(1);
                            if stopped_polls >= 2 {
                                if near_end {
                                    arm.feed.finish();
                                    let _ = arm.pipeline.set_state(gst::State::Null);
                                    if arm.finish_emit.swap(false, Ordering::SeqCst) {
                                        arm.eos.store(true, Ordering::SeqCst);
                                        publish_hq_output(
                                            &arm.app,
                                            &arm.watch,
                                            None,
                                            (&arm.gen_cell, arm.gen),
                                            arm.playback_generation,
                                        );
                                        let _ = arm.app.emit("track-finished", ());
                                    }
                                } else if arm.finish_emit.load(Ordering::SeqCst) {
                                    arm.feed.cancel();
                                    let _ = arm.pipeline.set_state(gst::State::Null);
                                    hq_emit_error(
                                        &arm,
                                        "HQPlayer stopped before the end of the track",
                                    );
                                    arm.finish_emit.store(false, Ordering::SeqCst);
                                }
                                return;
                            }
                        } else if readers == 0
                            && idle_since.elapsed() >= std::time::Duration::from_secs(12)
                        {
                            hq_did_not_start(&arm);
                            return;
                        }
                    }
                    _ => {
                        if !started
                            && readers == 0
                            && idle_since.elapsed() >= std::time::Duration::from_secs(12)
                        {
                            hq_did_not_start(&arm);
                            return;
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

// ── Appsink pipeline builder ───────────────────────────────────────────

/// audioconvert `mix-matrix` that maps a stereo source (in0=L, in1=R) onto the
/// first two of `out_channels` outputs at unity gain, silencing the rest. This
/// keeps L/R bit-exact on the device's first output pair (the monitor outs) and
/// fills the extra channels with digital silence — the GStreamer equivalent of
/// an ALSA `ttable.0.0 1; ttable.1.1 1` route. Coefficient leaves MUST be f32
/// (the property's leaf type is G_TYPE_FLOAT; f64 is rejected).
#[cfg(target_os = "linux")]
fn stereo_pad_mix_matrix(out_channels: u32) -> gst::Array {
    let rows = (0..out_channels).map(|o| {
        let cols = (0..2u32).map(move |i| if o == i { 1.0f32 } else { 0.0f32 });
        gst::Array::new(cols)
    });
    gst::Array::new(rows)
}

type PipelineParts = (gst::Pipeline, Option<gst::Element>, Option<gst::Element>);

#[cfg(target_os = "linux")]
struct AppSinkConfig<'a> {
    uri: &'a str,
    is_dash: bool,
    route: crate::proxy::Route,
    exclusive: bool,
    bit_perfect: bool,
    preserve_rate: bool,
    writer_tx: crossbeam_channel::Sender<WriterCommand>,
    writer_gen: Arc<AtomicU64>,
    negotiated_fmt: &'a PcmFormat,
    supported_gst_formats: &'a [&'a str],
    supported_rates: &'a [u32],
    decoded_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    output_cell: Arc<Mutex<Option<crate::pipeline_probe::PadCaps>>>,
    rejected: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

fn send_writer_data(
    tx: &crossbeam_channel::Sender<WriterCommand>,
    chunk: AudioChunk,
    cancelled: &AtomicBool,
) -> Result<(), gst::FlowError> {
    let mut command = WriterCommand::Data(chunk);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(gst::FlowError::Flushing);
        }
        match tx.send_timeout(command, std::time::Duration::from_millis(50)) {
            Ok(()) => return Ok(()),
            Err(crossbeam_channel::SendTimeoutError::Timeout(pending)) => command = pending,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                return Err(gst::FlowError::Error)
            }
        }
    }
}

fn reject_bit_perfect(pipe: &gst::Pipeline, rejected: &AtomicBool, cancelled: &AtomicBool) {
    cancelled.store(true, Ordering::Release);
    if !rejected.swap(true, Ordering::AcqRel) {
        gst::element_error!(pipe, gst::StreamError::Format,
            ("DAC cannot preserve the decoded format, rate or channels. Turn off bit-perfect to use compatible output."));
    }
}

#[cfg(target_os = "linux")]
fn build_appsink_pipeline(config: AppSinkConfig<'_>) -> Result<PipelineParts, String> {
    let AppSinkConfig {
        uri,
        is_dash,
        route,
        exclusive,
        bit_perfect,
        preserve_rate,
        writer_tx,
        writer_gen,
        negotiated_fmt,
        supported_gst_formats,
        supported_rates,
        decoded_cell,
        output_cell,
        rejected,
        cancelled,
    } = config;
    use gst_app::prelude::*;

    let pipe = gst::Pipeline::new();
    watch_pipeline_sources(&pipe, route);
    let mut udb = gst::ElementFactory::make("uridecodebin").property("uri", uri);
    if is_dash {
        udb = udb
            .property("buffer-duration", 15_000_000_000i64)
            .property("use-buffering", true);
    } else {
        udb = udb
            .property("buffer-duration", 5_000_000_000i64)
            .property("use-buffering", true);
    }
    let uridecodebin = udb
        .build()
        .map_err(|e| format!("Failed to create uridecodebin: {e}"))?;
    let device_channels = negotiated_fmt.channels;
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| format!("Failed to create audioconvert: {e}"))?;
    // Fixed-channel DAC: the device opened at > 2ch but the source is stereo.
    // Install a stereo→Nch silence-pad mix-matrix at BUILD time (before caps
    // negotiate) so the first caps event resolves directly to the device count
    // — avoids a 2ch transient that would thrash the ALSA writer.
    if device_channels > 2 {
        audioconvert.set_property("mix-matrix", stereo_pad_mix_matrix(device_channels));
        log::info!(
            "[audio] stereo→{device_channels}ch silence-pad mix-matrix installed for fixed-channel DAC"
        );
    }

    let appsink = gst_app::AppSink::builder()
        .max_buffers(20)
        .sync(false)
        .build();

    // DASH: constrain appsink to DAC-supported formats (and, for non-bit-perfect,
    // rates) for BOTH modes. The pad_added capsfilter relock is gated by
    // `if !is_dash` (DASH renegotiates caps mid-stream and would fight the lock),
    // so without this constraint a non-bit-perfect DASH chain has no protection
    // and source-format chunks (e.g. S24_32LE on a DAC that only supports S32LE)
    // reach the writer, triggering the strict format-mismatch teardown in the
    // Data handler.
    //
    // RATE is constrained ONLY in non-bit-perfect mode. There, an audioresample
    // element bridges any source rate to a DAC-supported one, so the constraint
    // is always satisfiable. In bit-perfect mode there is deliberately NO
    // resampler (audioconvert can't change rate), so pinning the rate to the
    // DAC's list makes GStreamer fail negotiation outright when the source rate
    // isn't supported — surfacing as the opaque "Internal data stream error" on
    // the bus instead of the actionable "turn off bit-perfect" message. Leaving
    // rate unconstrained lets the source rate pass through to the appsink; its
    // CAPS probe then hands the writer a FormatHint, the writer attempts the ALSA
    // reopen at that exact rate, and configure_alsa_hwparams emits the friendly
    // "DAC doesn't support XkHz — turn off bit-perfect mode" toast (matching the
    // non-DASH/BTS path).
    if is_dash {
        let mut caps_builder = gst::Caps::builder("audio/x-raw")
            .field(
                "format",
                gst::List::new(supported_gst_formats.iter().copied()),
            )
            .field("channels", device_channels as i32);
        let rate_list: Vec<i32> = supported_rates.iter().map(|&r| r as i32).collect();
        if !bit_perfect && !preserve_rate && !rate_list.is_empty() {
            caps_builder = caps_builder.field("rate", gst::List::new(rate_list));
        }
        appsink.set_caps(Some(&caps_builder.build()));
        log::debug!(
            "[audio] DASH appsink caps = formats:{:?} rates:{} (bit_perfect={bit_perfect})",
            supported_gst_formats,
            if bit_perfect {
                "passthrough".to_string()
            } else {
                format!("{supported_rates:?}")
            }
        );
    }

    log::debug!(
        "[audio] building appsink pipeline: exclusive={exclusive} bit_perfect={bit_perfect}"
    );

    let (u_vol, n_vol, capsfilter_weak_from_build): (
        Option<gst::Element>,
        Option<gst::Element>,
        Option<gst::glib::WeakRef<gst::Element>>,
    ) = if bit_perfect || preserve_rate {
        audioconvert.set_property_from_str("dithering", "none");
        audioconvert.set_property_from_str("noise-shaping", "none");

        {
            // Both transports pin formats on every decoded CAPS event.
            // BTS: capsfilter for dynamic locking (preserves exact decoded format)
            let capsfilter = gst::ElementFactory::make("capsfilter")
                .build()
                .map_err(|e| format!("Failed to create capsfilter: {e}"))?;
            let cf_weak = capsfilter.downgrade();
            pipe.add_many([
                &uridecodebin,
                &audioconvert,
                &capsfilter,
                appsink.upcast_ref(),
            ])
            .map_err(|e| format!("Failed to add elements: {e}"))?;
            gst::Element::link_many([&audioconvert, &capsfilter, appsink.upcast_ref()])
                .map_err(|e| format!("Failed to link bit-perfect chain: {e}"))?;
            (None, None, Some(cf_weak))
        }
    } else {
        // Exclusive (non-bit-perfect): volume applied in ALSA writer thread.
        // Rate constrained to DAC-supported rates — audioresample converts unsupported rates.
        let audioresample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|e| format!("Failed to create audioresample: {e}"))?;
        // Construct capsfilter EMPTY so it imposes no FORMAT constraint until
        // pad_added relocks it with the chosen format. Seeding a format here
        // makes src_pad.link() trigger downstream negotiation against the seed
        // BEFORE the relock runs — audioconvert then commits to converting (e.g.
        // S16LE→S32LE) and the writer reopens ALSA at the wrong format. (The
        // channel count is handled separately by the build-time mix-matrix,
        // which is orthogonal to format negotiation.) Matches the bit-perfect
        // BTS pattern where the capsfilter is also built empty.
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .build()
            .map_err(|e| format!("Failed to create capsfilter: {e}"))?;
        let cf_weak = capsfilter.downgrade();

        pipe.add_many([
            &uridecodebin,
            &audioconvert,
            &audioresample,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .map_err(|e| format!("Failed to add elements: {e}"))?;
        gst::Element::link_many([
            &audioconvert,
            &audioresample,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .map_err(|e| format!("Failed to link exclusive chain: {e}"))?;

        (None, None, Some(cf_weak))
    };

    // Capsfilter weak ref captured at element creation (line ~1903/1925).
    // DON'T use audioconvert.src.peer.parent_element — the chain length differs
    // between bit-perfect (audioconvert→capsfilter) and non-bit-perfect
    // (audioconvert→audioresample→capsfilter), so peer-walk would target the
    // wrong element in non-bit-perfect mode.
    let capsfilter_weak = capsfilter_weak_from_build;

    // Pad probe on audioconvert.sink — captures the codec's raw output
    // (pre-conversion). audioconvert.src would show the post-promotion
    // format when the downstream capsfilter is locked, which is misleading.
    if let Some(sink_pad) = audioconvert.static_pad("sink") {
        let cell = Arc::clone(&decoded_cell);
        let filter = capsfilter_weak.clone();
        let formats: Vec<String> = supported_gst_formats
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pipe_weak = pipe.downgrade();
        let rejected = Arc::clone(&rejected);
        let cancelled = Arc::clone(&cancelled);
        let promotion_tx = writer_tx.clone();
        let generation = Arc::clone(&writer_gen);
        sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if let gst::EventView::Caps(caps_event) = event.view() {
                    let format = parse_pcm_format(caps_event.caps());
                    *cell.lock().unwrap() =
                        format.as_ref().map(|fmt| crate::pipeline_probe::PadCaps {
                            format: fmt.gst_format.clone(),
                            rate: fmt.sample_rate,
                            channels: fmt.channels,
                        });
                    if bit_perfect {
                        let selected = format.as_ref().and_then(|fmt| {
                            if !(fmt.channels == device_channels
                                || (fmt.channels == 2 && device_channels > 2))
                            {
                                return None;
                            }
                            pick_lossless_format(&fmt.gst_format, &formats)
                                .map(|chosen| (fmt, chosen))
                        });
                        let Some((fmt, chosen)) = selected else {
                            if let Some(pipe) = pipe_weak.upgrade() {
                                reject_bit_perfect(&pipe, &rejected, &cancelled);
                            }
                            return gst::PadProbeReturn::Drop;
                        };
                        if let Some(cf) = filter.as_ref().and_then(|f| f.upgrade()) {
                            cf.set_property(
                                "caps",
                                gst::Caps::builder("audio/x-raw")
                                    .field("format", chosen.as_str())
                                    .field("rate", fmt.sample_rate as i32)
                                    .field("channels", device_channels as i32)
                                    .build(),
                            );
                        }
                        let _ = promotion_tx.try_send(WriterCommand::PendingPromotion {
                            from: fmt.gst_format.clone(),
                            generation: generation.load(Ordering::Acquire),
                        });
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    // Connect uridecodebin's dynamic pad to audioconvert
    let convert_weak = audioconvert.downgrade();
    let supported_fmts_for_closure: Vec<String> = supported_gst_formats
        .iter()
        .map(|s| s.to_string())
        .collect();
    let supported_rates_for_closure: Vec<u32> = supported_rates.to_vec();
    let resample_tx = writer_tx.clone();
    let is_bit_perfect = bit_perfect;
    uridecodebin.connect_pad_added(move |_src, src_pad| {
        let Some(convert) = convert_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = convert.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }

        if let Some(caps) = src_pad.current_caps() {
            if let Some(s) = caps.structure(0) {
                if !s.name().as_str().starts_with("audio/") {
                    return;
                }
            }
        }

        if let Err(e) = src_pad.link(&sink_pad) {
            log::error!("Failed to link uridecodebin pad: {e:?}");
        }

        // Detect if resampling will occur (non-bit-perfect exclusive only)
        if !is_bit_perfect && !preserve_rate {
            if let Some(caps) = src_pad.current_caps() {
                if let Some(s) = caps.structure(0) {
                    if let Ok(native_rate) = s.get::<i32>("rate") {
                        let native = native_rate as u32;
                        if !supported_rates_for_closure.contains(&native) {
                            let closest = supported_rates_for_closure
                                .iter()
                                .copied()
                                .min_by_key(|&r| (r as i64 - native as i64).unsigned_abs())
                                .unwrap_or(48000);
                            let _ = resample_tx.try_send(WriterCommand::Resampling {
                                from: native,
                                to: closest,
                            });
                        }
                    }
                }
            }
        }

        // Compatibility mode can choose a lossy format or resample to a rate
        // supported by the DAC. Strict mode locks every decoded CAPS event above.
        if (!is_dash || preserve_rate) && !is_bit_perfect {
            let caps = src_pad.current_caps().or_else(|| {
                let query = src_pad.query_caps(None);
                if query.is_fixed() {
                    Some(query)
                } else {
                    None
                }
            });
            if let Some(caps) = caps {
                if let Some(s) = caps.structure(0) {
                    if let (Ok(rate), Ok(channels), Ok(format)) = (
                        s.get::<i32>("rate"),
                        s.get::<i32>("channels"),
                        s.get::<&str>("format"),
                    ) {
                        // The build-time mix-matrix assumes a stereo source (SONE
                        // only ever streams stereo). Surface it loudly if a
                        // non-stereo source ever reaches a multichannel-only DAC,
                        // where the [device][2] matrix would fail to negotiate.
                        if device_channels > 2 && channels != 2 {
                            log::error!(
                                "[audio] {channels}ch source on a {device_channels}ch-only DAC: \
                                 stereo-pad mix-matrix cannot negotiate this layout"
                            );
                        }

                        let chosen = pick_capsfilter_format(format, &supported_fmts_for_closure);

                        if let Some(ref cf_weak) = capsfilter_weak {
                            if let Some(cf) = cf_weak.upgrade() {
                                // Keep a rate list so audioresample can select a
                                // supported rate when the native one is unavailable.
                                let rate_list: Vec<i32> = if preserve_rate {
                                    vec![rate]
                                } else {
                                    supported_rates_for_closure
                                        .iter()
                                        .map(|&r| r as i32)
                                        .collect()
                                };
                                let locked = gst::Caps::builder("audio/x-raw")
                                    .field("format", chosen.as_str())
                                    .field("channels", device_channels as i32)
                                    .field("rate", gst::List::new(rate_list))
                                    .build();
                                log::info!("[audio] capsfilter locked to {locked}");
                                cf.set_property("caps", &locked);

                                // Belt-and-braces: notify the writer explicitly so it
                                // reopens ALSA at the chosen format. The appsink CAPS
                                // probe will also fire FormatHint when the new caps
                                // event reaches it; both arrive at the writer's mpsc
                                // and the writer dedups via the current_fmt comparison.
                                let bps: u32 = match chosen.as_str() {
                                    "S16LE" => 2,
                                    "S24LE" => 3,
                                    _ => 4,
                                };
                                let hint_fmt = PcmFormat {
                                    gst_format: chosen.clone(),
                                    sample_rate: rate as u32,
                                    channels: device_channels,
                                    bytes_per_sample: bps,
                                };
                                let _ = resample_tx.try_send(WriterCommand::FormatHint(hint_fmt));
                            }
                        }
                    }
                }
            }
        }
    });

    // Pad probe: intercept CAPS events for preemptive ALSA format changes (DASH renegotiation)
    let probe_tx = writer_tx.clone();
    if let Some(sink_pad) = appsink.static_pad("sink") {
        let output_cell_for_probe = Arc::clone(&output_cell);
        let decoded = Arc::clone(&decoded_cell);
        let rejected = Arc::clone(&rejected);
        let cancelled = Arc::clone(&cancelled);
        let pipe_weak = pipe.downgrade();
        sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if let gst::EventView::Caps(caps_event) = event.view() {
                    let format = parse_pcm_format(caps_event.caps());
                    if bit_perfect
                        && !format.as_ref().is_some_and(|fmt| {
                            decoded.lock().unwrap().as_ref().is_some_and(|source| {
                                source.rate == fmt.sample_rate
                                    && (source.channels == fmt.channels
                                        || (source.channels == 2 && fmt.channels > 2))
                                    && sample_format_preserved(&source.format, &fmt.gst_format)
                            })
                        })
                    {
                        if let Some(pipe) = pipe_weak.upgrade() {
                            reject_bit_perfect(&pipe, &rejected, &cancelled);
                        }
                        return gst::PadProbeReturn::Drop;
                    }
                    if let Some(fmt) = format {
                        log::debug!("[audio] CAPS event on appsink: {fmt:?}");
                        if let Ok(mut guard) = output_cell_for_probe.lock() {
                            *guard = Some(crate::pipeline_probe::PadCaps {
                                format: fmt.gst_format.clone(),
                                rate: fmt.sample_rate,
                                channels: fmt.channels,
                            });
                        }
                        let _ = probe_tx.try_send(WriterCommand::FormatHint(fmt));
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    // Appsink callback: extract PCM and forward to ALSA writer
    let chunk_gen = Arc::clone(&writer_gen);
    let pipe_weak = pipe.downgrade();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let format = sample.caps().and_then(parse_pcm_format).ok_or_else(|| {
                    if bit_perfect {
                        if let Some(pipe) = pipe_weak.upgrade() {
                            reject_bit_perfect(&pipe, &rejected, &cancelled);
                        }
                    }
                    gst::FlowError::Error
                })?;
                if bit_perfect
                    && (rejected.load(Ordering::Acquire)
                        || !decoded_cell.lock().unwrap().as_ref().is_some_and(|source| {
                            let decoded = PcmFormat {
                                gst_format: source.format.clone(),
                                sample_rate: source.rate,
                                channels: source.channels,
                                bytes_per_sample: 0,
                            };
                            samples_preserved(&decoded, &format)
                        }))
                {
                    if let Some(pipe) = pipe_weak.upgrade() {
                        reject_bit_perfect(&pipe, &rejected, &cancelled);
                    }
                    return Err(gst::FlowError::Error);
                }
                if cancelled.load(Ordering::Acquire) {
                    return Err(gst::FlowError::Flushing);
                }

                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let data = map.as_slice().to_vec();
                let generation = chunk_gen.load(Ordering::Acquire);

                send_writer_data(
                    &writer_tx,
                    AudioChunk {
                        data,
                        format,
                        generation,
                    },
                    &cancelled,
                )?;

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    Ok((pipe, u_vol, n_vol))
}

// ── Device enumeration ─────────────────────────────────────────────────

/// Enumerate ALSA hardware devices. Does NOT use the audio pipeline,
/// so it is safe to call from any thread.
pub fn list_alsa_devices() -> Result<Vec<AudioDevice>, String> {
    list_alsa_devices_inner()
}

fn list_alsa_devices_inner() -> Result<Vec<AudioDevice>, String> {
    gst::init().map_err(|e| format!("GStreamer init failed: {e}"))?;
    let monitor = gst::DeviceMonitor::new();
    let caps = gst::Caps::new_empty_simple("audio/x-raw");
    monitor.add_filter(Some("Audio/Sink"), Some(&caps));
    monitor
        .start()
        .map_err(|e| format!("Failed to start device monitor: {e}"))?;

    // GStreamer 1.28+ starts providers async, so devices() may initially be empty.
    // On older versions start() blocks and devices are available immediately.
    let devices = {
        let mut devs = monitor.devices();
        let mut waited = 0u32;
        while devs.is_empty() && waited < 2000 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            devs = monitor.devices();
            waited += 100;
        }
        devs
    };

    monitor.stop();

    log::debug!(
        "[list_alsa_devices] DeviceMonitor found {} devices",
        devices.len()
    );

    let mut result = Vec::new();
    for dev in &devices {
        let Some(props) = dev.properties() else {
            continue;
        };

        let api = props.get::<String>("device.api").unwrap_or_default();
        if api != "alsa" {
            continue;
        }

        let path = props.get::<String>("api.alsa.path").ok().or_else(|| {
            let card = props.get::<String>("alsa.card").ok()?;
            let dev_num = props.get::<String>("alsa.device").ok()?;
            Some(format!("hw:{card},{dev_num}"))
        });

        if let Some(path) = path {
            let name = dev.display_name().to_string();
            log::debug!("[list_alsa_devices] found: '{}' -> {}", name, path);
            result.push(AudioDevice { id: path, name });
        }
    }

    log::debug!("[list_alsa_devices] returning {} devices", result.len());
    Ok(result)
}

/// Gapless (2b architecture) needs the `concat` element. The chain is legacy
/// `uridecodebin` then a per-branch `queue` then `concat`, which handle Tidal
/// `data:application/dash+xml` on any GStreamer with the legacy dash demuxer
/// (no GStreamer 1.24 or uridecodebin3 requirement). `concat` ships in
/// coreelements, so this is effectively always true.
pub fn gapless_supported() -> bool {
    gst::ElementFactory::find("concat").is_some()
}

#[cfg(test)]
mod proxy_source_tests {
    use super::*;
    use crate::proxy::{BlockReason, Capability, HostCaps, Route};

    #[test]
    fn the_probe_reports_the_registry_not_the_stand_in() {
        let probed = probe_host_caps();
        let (major, minor, micro, _nano) = gst::version();

        // The stand-in claims exactly the version floor, so asserting a
        // plausible version would pass against it. Assert the real one.
        assert_eq!(
            probed.gst_version,
            (major, minor, micro),
            "probe must report the linked GStreamer, not an assumption"
        );
        assert_eq!(
            probed.has_dashdemux,
            gst::ElementFactory::find("dashdemux").is_some()
        );
        assert_eq!(
            probed.has_curlhttpsrc,
            gst::ElementFactory::find("curlhttpsrc").is_some()
        );
    }

    fn http_proxy() -> crate::ProxySettings {
        crate::ProxySettings {
            enabled: true,
            proxy_type: crate::ProxyType::Http,
            host: "proxy.example".into(),
            port: 3128,
            username: None,
            password: None,
        }
    }

    #[test]
    fn a_usable_proxy_routes_both_tiers_identically() {
        let p = AudioProxy::new(http_proxy(), HostCaps::assume_all_present());
        let lossy = p
            .route_for(Capability::Lossy)
            .expect("lossy is routable here");
        let dash = p
            .route_for(Capability::Dash)
            .expect("dash is routable here");
        // They share one scheme arm, so when both succeed they are the same
        // value. This test exists to catch the day that stops being true.
        assert_eq!(lossy, dash);
        assert!(matches!(lossy, Route::Via { .. }));
    }

    #[test]
    fn a_tier_that_cannot_be_proxied_is_an_error_not_a_direct_route() {
        // The defect this type exists to prevent: collapsing "blocked" into
        // "no proxy" lets a refused tier stream on the user's own address.
        let mut caps = HostCaps::assume_all_present();
        caps.has_dashdemux = false;
        let p = AudioProxy::new(http_proxy(), caps);

        assert!(matches!(
            p.route_for(Capability::Dash),
            Err(BlockReason { .. })
        ));
        assert!(
            p.route_for(Capability::Lossy).is_ok(),
            "lossy proxies fine here and must keep playing"
        );
    }

    #[test]
    fn settings_that_form_no_plan_are_an_error_not_a_direct_route() {
        let mut bad = http_proxy();
        bad.port = 0;
        let p = AudioProxy::new(bad, HostCaps::assume_all_present());
        for c in [Capability::Lossy, Capability::Dash] {
            assert!(matches!(p.route_for(c), Err(BlockReason { .. })));
        }
    }

    #[test]
    fn a_disabled_proxy_routes_directly_and_is_never_an_error() {
        let mut off = http_proxy();
        off.enabled = false;
        let p = AudioProxy::new(off, HostCaps::assume_all_present());
        for c in [Capability::Lossy, Capability::Dash] {
            assert_eq!(
                p.route_for(c).expect("direct is not a refusal"),
                Route::NoProxy
            );
        }
    }

    use crate::proxy::Creds;

    fn make(name: &str) -> Option<gst::Element> {
        let _ = gst::init();
        gst::ElementFactory::make(name).build().ok()
    }

    fn via(creds: Option<Creds>) -> Route {
        Route::Via {
            uri: "http://proxy.example:3128".into(),
            creds,
        }
    }

    #[test]
    fn both_credential_properties_are_set_together() {
        let src = make("souphttpsrc").expect("souphttpsrc is a base dependency");
        apply_route_to_source(
            &src,
            &via(Some(Creds {
                user: "bob".into(),
                pass: String::new(),
            })),
        );
        // This element normalizes the URI through GstUri on write, so the
        // readback gains a trailing slash. Measured, not assumed.
        assert_eq!(
            src.property::<Option<String>>("proxy").as_deref(),
            Some("http://proxy.example:3128/")
        );
        // It authenticates only when it has both properties, so an empty
        // password must still produce a pair rather than half of one.
        assert_eq!(
            src.property::<Option<String>>("proxy-id").as_deref(),
            Some("bob")
        );
        assert_eq!(
            src.property::<Option<String>>("proxy-pw").as_deref(),
            Some("")
        );
    }

    #[test]
    fn the_curl_source_receives_the_uri_byte_exact() {
        let Some(src) = make("curlhttpsrc") else {
            panic!("curlhttpsrc missing: it is a declared dependency of this app");
        };
        // Unlike souphttpsrc this one does not rewrite the value, so it is the
        // element that proves the port survives. Port 80 reaching libcurl
        // without its port is the original defect: libcurl then dials 1080.
        apply_route_to_source(
            &src,
            &Route::Via {
                uri: "http://127.0.0.1:80".into(),
                creds: None,
            },
        );
        assert_eq!(
            src.property::<Option<String>>("proxy").as_deref(),
            Some("http://127.0.0.1:80")
        );
    }

    #[test]
    fn a_noproxy_route_leaves_the_source_alone() {
        let src = make("souphttpsrc").expect("souphttpsrc is a base dependency");
        apply_route_to_source(&src, &Route::NoProxy);
        // This element's `proxy` defaults to an empty string, not NULL.
        assert!(src
            .property::<Option<String>>("proxy")
            .unwrap_or_default()
            .is_empty());
    }

    #[test]
    fn the_curl_source_gets_a_bounded_timeout_and_retry_count() {
        let Some(src) = make("curlhttpsrc") else {
            panic!("curlhttpsrc missing: it is a declared dependency of this app");
        };
        // Left at 0 and -1 a dead-but-reachable proxy produces no bus error at
        // all: playback stalls and nothing can be reported. Both are `gint` on
        // 1.24 and 1.26 alike — a `u32` panics on the streaming thread.
        apply_route_to_source(&src, &via(None));
        assert_eq!(src.property::<i32>("timeout"), 15);
        assert_eq!(src.property::<i32>("retries"), 3);
    }

    #[test]
    fn a_source_without_proxy_properties_is_left_untouched() {
        let src = make("dataurisrc").expect("dataurisrc is a base dependency");
        // The manifest for high-resolution audio arrives as a `data:` URI, and
        // the hook does see this element. `set_property` panics on a property
        // an element lacks, so the factory filter is load-bearing.
        apply_route_to_source(&src, &via(None));
    }

    #[test]
    fn the_autoplugged_https_source_is_one_we_can_configure() {
        let _ = gst::init();
        // If this is ever false on a supported host, the build sites refuse and
        // the user is told -- rather than streaming direct through a source
        // whose `proxy` property was never set because we did not recognise it.
        assert!(
            http_source_is_configurable(),
            "the winning https source is outside HTTP_SOURCE_FACTORIES"
        );
    }

    #[test]
    fn the_curl_source_is_promoted_only_while_credentials_are_in_use() {
        let _ = gst::init();
        let Some(curl) = gst::ElementFactory::find("curlhttpsrc") else {
            panic!("curlhttpsrc missing: it is a declared dependency of this app");
        };
        let original = curl.rank();

        promote_curl_source(
            &via(Some(Creds {
                user: "bob".into(),
                pass: "hunter2".into(),
            })),
            Some(original),
        );
        assert!(gst::ElementFactory::find("curlhttpsrc").unwrap().rank() > original);

        // Without credentials the soup source is correct: it handles more
        // authentication schemes and ignores an ambient `no_proxy`.
        promote_curl_source(&via(None), Some(original));
        assert_eq!(
            gst::ElementFactory::find("curlhttpsrc").unwrap().rank(),
            original,
            "rank must return to its pristine value, not merely go down"
        );

        promote_curl_source(&Route::NoProxy, Some(original));
        assert_eq!(
            gst::ElementFactory::find("curlhttpsrc").unwrap().rank(),
            original
        );
    }

    #[test]
    fn a_blocked_tier_yields_no_route_to_hand_a_pipeline() {
        // The contract the build sites rely on: when a tier is refused there is
        // no `Route` value at all, so there is nothing to accidentally apply.
        let mut caps = HostCaps::assume_all_present();
        caps.has_dashdemux = false;
        let p = AudioProxy::new(http_proxy(), caps);

        let err = p
            .route_for(capability_of(true))
            .expect_err("dash must be refused without the legacy demuxer");
        assert!(
            !err.cause.is_empty(),
            "a refusal must carry a reason to show"
        );

        p.route_for(capability_of(false))
            .expect("lossy must still play");
    }

    #[test]
    fn the_dash_flag_is_not_inverted_on_the_way_to_a_capability() {
        // Both build sites derive the flag from this one prefix and hand the
        // result to `capability_of`. Inverting it there is silent: the refusals
        // still fire, just for the wrong tier, so hi-res would stream direct on
        // a host where only lossy is routable.
        let dash_uri = "data:application/dash+xml;base64,PE1QRD4=";
        let lossy_uri = "https://audio.example/track.flac";

        assert!(dash_uri.starts_with("data:application/dash"));
        assert!(!lossy_uri.starts_with("data:application/dash"));

        assert_eq!(
            capability_of(dash_uri.starts_with("data:application/dash")),
            Capability::Dash
        );
        assert_eq!(
            capability_of(lossy_uri.starts_with("data:application/dash")),
            Capability::Lossy
        );
    }

    #[test]
    fn the_pipeline_hook_configures_a_source_added_after_it_was_installed() {
        let _ = gst::init();
        // The hook is the whole containment mechanism for both build sites, and
        // it runs on the streaming thread where a failure is invisible. Adding
        // the source *after* the connect is the case that matters: every real
        // source appears long after the pipeline is built.
        let pipe = gst::Pipeline::new();
        watch_pipeline_sources(
            &pipe,
            via(Some(Creds {
                user: "bob".into(),
                pass: "hunter2".into(),
            })),
        );

        let src = make("souphttpsrc").expect("souphttpsrc is a base dependency");
        let bin = gst::Bin::new();
        bin.add(&src).expect("bin accepts the source");
        // Nested, so this also proves the signal reaches through a sub-bin --
        // which is how an adaptive demuxer's segment source actually arrives.
        pipe.add(&bin).expect("pipeline accepts the bin");

        assert_eq!(
            src.property::<Option<String>>("proxy").as_deref(),
            Some("http://proxy.example:3128/"),
            "a source added after the hook was installed must still be routed"
        );
        assert_eq!(
            src.property::<Option<String>>("proxy-id").as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn a_stale_pipeline_is_refused_before_the_next_branch_is_prerolled() {
        let _ = gst::init();
        // `SetProxySettings` bumps the generation and only then queues the
        // rebuild on the self-sender, so a `SetNextTrack` landing in that gap is
        // served by the pipeline that is about to be replaced — whose hook still
        // applies the route the user just left. The refusal therefore has to
        // come before `attach_next_bin`, which prerolls up to fifteen seconds of
        // the next track.
        let Some(concat) = make("concat") else {
            panic!("concat ships in coreelements, and gapless is gated on it");
        };
        let pipeline = gst::Pipeline::new();
        pipeline.add(&concat).expect("pipeline accepts concat");

        let next_bin: Arc<Mutex<Option<NextBinState>>> = Arc::new(Mutex::new(None));
        let audio_proxy = Arc::new(Mutex::new(AudioProxy::new(
            crate::ProxySettings::default(),
            HostCaps::assume_all_present(),
        )));
        let route_generation = Arc::new(AtomicU64::new(7));
        let (tx, rx) = mpsc::channel::<AttachJob>();
        let executor = {
            let next_bin = Arc::clone(&next_bin);
            let route_generation = Arc::clone(&route_generation);
            std::thread::spawn(move || {
                run_attach_executor(
                    rx,
                    next_bin,
                    audio_proxy,
                    route_generation,
                    Arc::new(AtomicBool::new(false)),
                )
            })
        };

        // A file that does not exist: nothing is fetched, and the preroll
        // failing changes nothing here — `attach_next_bin` reports Ok once the
        // branch is linked into the pipeline.
        let job = |build_generation: u64, track_id: u64| AttachJob::Attach {
            pipeline: pipeline.clone(),
            concat: concat.clone(),
            build_generation,
            uri: "file:///nonexistent/sone-gapless-guard.flac".into(),
            is_dash: false,
            track_id,
            qid: track_id.to_string(),
            norm_gain: 1.0,
            replay_gain: f64::NAN,
            peak_amplitude: f64::NAN,
        };

        // The first job carries the current generation and must attach. Without
        // it a refusal proves nothing: a branch that could not be built here at
        // all would leave the same empty pipeline behind.
        tx.send(job(7, 1)).expect("the executor is running");
        tx.send(job(6, 2)).expect("the executor is running");
        drop(tx);
        executor
            .join()
            .expect("the executor thread finishes with the sender");

        let armed = next_bin
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("the current-generation job must arm the slot");
        assert_eq!(
            armed.track_id, 1,
            "a job stamped with an older pipeline must not take the slot"
        );
        assert_eq!(
            pipeline.children().len(),
            3,
            "the stale job must add nothing: concat plus one branch (a \
             uridecodebin and its queue) is everything that may be here"
        );

        detach_bin(&pipeline, &concat, &armed.bin, &armed.branch_queue);
    }

    #[test]
    fn a_settings_change_that_alters_the_route_requires_a_rebuild() {
        let caps = HostCaps::assume_all_present();
        let off = AudioProxy::new(
            crate::ProxySettings {
                enabled: false,
                ..http_proxy()
            },
            caps,
        );
        let on = AudioProxy::new(http_proxy(), caps);
        assert!(
            audio_route_differs(&off, &on),
            "off -> proxied must force a rebuild"
        );
    }

    #[test]
    fn off_to_blocked_is_detected_as_a_change() {
        // The comparison a bare `Route` cannot make: both sides would spell
        // themselves `NoProxy`, so a route-equality test reports "no change"
        // and the direct stream continues.
        let caps = HostCaps::assume_all_present();
        let off = AudioProxy::new(
            crate::ProxySettings {
                enabled: false,
                ..http_proxy()
            },
            caps,
        );
        let blocked = AudioProxy::new(
            crate::ProxySettings {
                port: 0,
                ..http_proxy()
            },
            caps,
        );
        assert!(
            audio_route_differs(&off, &blocked),
            "off -> blocked must force a teardown, not be mistaken for no change"
        );
    }

    #[test]
    fn one_blocked_configuration_to_a_different_one_is_still_a_change() {
        // On any host below GStreamer 1.26.10 -- every non-Flatpak target in the
        // spec's table -- `route(Lossy)` is permanently Err for every credentialed
        // proxy. A comparison that treats two refusals as equal would let a user
        // switch from proxy A to proxy B mid-track and keep streaming through A.
        let mut caps = HostCaps::assume_all_present();
        caps.gst_version = (1, 24, 2);
        let with_creds = |host: &str| crate::ProxySettings {
            username: Some("bob".into()),
            password: Some("hunter2".into()),
            host: host.into(),
            ..http_proxy()
        };
        let a = AudioProxy::new(with_creds("proxy-a.example"), caps);
        let b = AudioProxy::new(with_creds("proxy-b.example"), caps);
        assert!(
            a.route_for(Capability::Lossy).is_err(),
            "precondition: lossy is gated below 1.26.10 when credentials are present"
        );
        // `BlockReason` carries no host, so the two Lossy refusals are byte
        // identical -- it is the `Dash` arm that catches this, which is exactly
        // why both capabilities have to be compared. A single-capability check
        // on `Lossy` would report "no change" and keep streaming through A.
        assert!(
            audio_route_differs(&a, &b),
            "a different blocked configuration is still a change"
        );
    }

    #[test]
    fn an_ambient_bypass_list_at_launch_blocks_audio_until_restart() {
        // `curlhttpsrc` reads `no_proxy` when the element is constructed and
        // forwards it as CURLOPT_NOPROXY, which overrides the `proxy` property
        // we set -- measured going direct. The variable can only be removed
        // before GTK threads exist, so within this session the honest answer
        // is to refuse and say why.
        let caps = HostCaps::assume_all_present();
        let p = AudioProxy::new(http_proxy(), caps).with_launch_bypass(true);

        for c in [Capability::Lossy, Capability::Dash] {
            let err = p.route_for(c).expect_err("a bypass list must block audio");
            assert!(
                err.cause.contains("restart"),
                "the reason must tell the user what to do, got: {}",
                err.cause
            );
        }
    }

    #[test]
    fn a_bypass_list_is_irrelevant_when_the_proxy_is_off() {
        // Direct means the system's own configuration applies -- including its
        // bypass list. Refusing here would break playback for a user who is not
        // proxying at all.
        let mut off = http_proxy();
        off.enabled = false;
        let p = AudioProxy::new(off, HostCaps::assume_all_present()).with_launch_bypass(true);
        for c in [Capability::Lossy, Capability::Dash] {
            assert_eq!(
                p.route_for(c).expect("direct is unaffected"),
                Route::NoProxy
            );
        }
    }

    mod reliability_tests {
        use super::*;
        use std::collections::VecDeque;
        use std::time::{Duration, Instant};

        struct FakePcm<'a> {
            now: Instant,
            writes: VecDeque<Result<usize, i32>>,
            written: Vec<u8>,
            frame_size: usize,
            resumes: VecDeque<Result<(), i32>>,
            prepare: Result<(), i32>,
            prepare_calls: usize,
            waits: Vec<u32>,
            sleeps: Vec<u32>,
            cancel_on_wait: Option<&'a AtomicBool>,
            cancel_on_sleep: Option<&'a AtomicBool>,
            unpause_after: Option<(Instant, &'a AtomicBool)>,
        }

        impl Default for FakePcm<'_> {
            fn default() -> Self {
                Self {
                    now: Instant::now(),
                    writes: VecDeque::new(),
                    written: vec![],
                    frame_size: 2,
                    resumes: VecDeque::new(),
                    prepare: Ok(()),
                    prepare_calls: 0,
                    waits: vec![],
                    sleeps: vec![],
                    cancel_on_wait: None,
                    cancel_on_sleep: None,
                    unpause_after: None,
                }
            }
        }
        impl PcmIo for FakePcm<'_> {
            fn write(&mut self, bytes: &[u8]) -> Result<usize, i32> {
                let result = self.writes.pop_front().unwrap_or(Err(libc::EAGAIN));
                if let Ok(frames) = result {
                    self.written
                        .extend_from_slice(&bytes[..frames * self.frame_size]);
                }
                result
            }
            fn wait(&mut self, millis: u32) -> Result<(), i32> {
                assert!(millis <= 50);
                self.waits.push(millis);
                self.now += Duration::from_millis(millis.into());
                if let Some(cancelled) = self.cancel_on_wait {
                    cancelled.store(true, Ordering::Release);
                }
                Ok(())
            }
            fn resume(&mut self) -> Result<(), i32> {
                self.resumes.pop_front().unwrap_or(Err(libc::EAGAIN))
            }
            fn prepare(&mut self) -> Result<(), i32> {
                self.prepare_calls += 1;
                self.prepare
            }
            fn now(&self) -> Instant {
                self.now
            }
            fn sleep(&mut self, millis: u32) {
                self.sleeps.push(millis);
                self.now += Duration::from_millis(millis.into());
                if let Some(cancelled) = self.cancel_on_sleep {
                    cancelled.store(true, Ordering::Release);
                }
                if let Some((deadline, paused)) = self.unpause_after {
                    if self.now >= deadline {
                        paused.store(false, Ordering::Release);
                    }
                }
            }
        }

        #[test]
        fn partial_writes_and_eagain_preserve_every_frame_once() {
            let mut io = FakePcm {
                writes: [Ok(1), Err(libc::EAGAIN), Ok(0), Ok(2)].into(),
                ..Default::default()
            };
            let frames = AtomicU64::new(0);
            let data = [1, 2, 3, 4, 5, 6];
            assert_eq!(
                write_pcm(
                    &mut io,
                    &data,
                    2,
                    &AtomicBool::new(false),
                    &AtomicBool::new(false),
                    &frames
                ),
                Ok(())
            );
            assert_eq!(io.written, data);
            assert_eq!(frames.load(Ordering::Relaxed), 3);
            assert_eq!(io.waits, [50, 50]);
        }

        #[test]
        fn no_progress_times_out_but_user_pause_does_not() {
            let cancelled = AtomicBool::new(false);
            let paused = AtomicBool::new(false);
            let frames = AtomicU64::new(0);
            let mut stalled = FakePcm::default();
            let start = stalled.now;
            assert_eq!(
                write_pcm(&mut stalled, &[1, 2], 2, &cancelled, &paused, &frames),
                Err("write_timeout")
            );
            assert_eq!(stalled.now.duration_since(start), Duration::from_secs(2));

            paused.store(true, Ordering::Release);
            let mut paused_io = FakePcm {
                writes: [Ok(1)].into(),
                ..Default::default()
            };
            paused_io.unpause_after = Some((paused_io.now + Duration::from_secs(8), &paused));
            assert_eq!(
                write_pcm(&mut paused_io, &[1, 2], 2, &cancelled, &paused, &frames),
                Ok(())
            );
            assert_eq!(paused_io.written, [1, 2]);
        }

        #[test]
        fn cancellation_interrupts_wait_and_suspend_recovery_without_an_audio_error() {
            let cancelled = AtomicBool::new(false);
            let mut waiting = FakePcm {
                cancel_on_wait: Some(&cancelled),
                ..Default::default()
            };
            assert_eq!(
                write_pcm(
                    &mut waiting,
                    &[1, 2],
                    2,
                    &cancelled,
                    &AtomicBool::new(false),
                    &AtomicU64::new(0)
                ),
                Err("cancelled")
            );
            assert_eq!(waiting.waits, [50]);
            cancelled.store(false, Ordering::Release);
            let mut suspended = FakePcm {
                cancel_on_sleep: Some(&cancelled),
                ..Default::default()
            };
            assert_eq!(
                recover_pcm(&mut suspended, libc::ESTRPIPE, &cancelled),
                Err("cancelled")
            );
            assert_eq!(suspended.sleeps, [20]);
        }

        #[test]
        fn recovery_reports_prepare_failure_and_bounds_suspend_retries() {
            let cancelled = AtomicBool::new(false);
            let mut failed = FakePcm {
                prepare: Err(libc::ENODEV),
                ..Default::default()
            };
            assert_eq!(
                recover_pcm(&mut failed, libc::EPIPE, &cancelled),
                Err("device_disconnected")
            );
            assert_eq!(failed.prepare_calls, 1);
            let mut suspended = FakePcm::default();
            let start = suspended.now;
            assert_eq!(
                recover_pcm(&mut suspended, libc::ESTRPIPE, &cancelled),
                Err("suspend_timeout")
            );
            assert_eq!(suspended.now.duration_since(start), Duration::from_secs(2));
            assert!(suspended.sleeps.iter().all(|&ms| ms == 20));
            let mut recovered = FakePcm {
                resumes: [Err(libc::EAGAIN), Ok(())].into(),
                ..Default::default()
            };
            assert_eq!(
                recover_pcm(&mut recovered, libc::ESTRPIPE, &cancelled),
                Ok(())
            );
            assert_eq!(recovered.prepare_calls, 0);
        }

        #[test]
        fn progress_resets_deadline_and_recovery_does_not_discard_pending_frames() {
            let mut writes = VecDeque::new();
            writes.extend(std::iter::repeat_n(Err(libc::EAGAIN), 30));
            writes.push_back(Ok(1));
            writes.push_back(Err(libc::EPIPE));
            writes.extend(std::iter::repeat_n(Err(libc::EAGAIN), 30));
            writes.push_back(Ok(1));
            let mut io = FakePcm {
                writes,
                ..Default::default()
            };
            let start = io.now;
            assert_eq!(
                write_pcm(
                    &mut io,
                    &[1, 2, 3, 4],
                    2,
                    &AtomicBool::new(false),
                    &AtomicBool::new(false),
                    &AtomicU64::new(0)
                ),
                Ok(())
            );
            assert_eq!(io.now.duration_since(start), Duration::from_secs(3));
            assert_eq!(io.written, [1, 2, 3, 4]);
            assert_eq!(io.prepare_calls, 1);
        }

        fn devices(name: &str) -> Vec<AudioDevice> {
            vec![AudioDevice {
                id: "hw:0".into(),
                name: name.into(),
            }]
        }

        #[test]
        fn device_cache_expires_refreshes_and_retries_failed_probes() {
            let cache = AudioDeviceCache::default();
            assert_eq!(
                cache.probe(false, || Ok(devices("first"))).unwrap()[0].name,
                "first"
            );
            assert_eq!(
                cache.probe(false, || panic!("cached")).unwrap()[0].name,
                "first"
            );
            assert_eq!(
                cache.probe(true, || Ok(devices("refreshed"))).unwrap()[0].name,
                "refreshed"
            );
            cache.result.lock().unwrap().as_mut().unwrap().0 =
                Instant::now() - Duration::from_secs(31);
            assert_eq!(
                cache.probe(false, || Ok(devices("expired"))).unwrap()[0].name,
                "expired"
            );
            assert_eq!(
                cache.probe(true, || Err("unavailable".into())).unwrap_err(),
                "unavailable"
            );
            assert_eq!(
                cache.probe(false, || Ok(devices("recovered"))).unwrap()[0].name,
                "recovered"
            );
        }

        #[test]
        fn concurrent_refresh_joins_success_and_error_from_the_in_flight_probe() {
            for result in [Ok(devices("shared")), Err("probe failed".to_string())] {
                let cache = Arc::new(AudioDeviceCache::default());
                let waiter_started = Instant::now();
                let (entered_tx, entered_rx) = mpsc::channel();
                let (finish_tx, finish_rx) = mpsc::channel();
                let first = {
                    let cache = Arc::clone(&cache);
                    std::thread::spawn(move || {
                        cache.probe(true, || {
                            entered_tx.send(()).unwrap();
                            finish_rx.recv().unwrap();
                            result
                        })
                    })
                };
                entered_rx.recv().unwrap();
                let waiter = {
                    let cache = Arc::clone(&cache);
                    std::thread::spawn(move || {
                        cache.probe_started(waiter_started, true, || panic!("duplicate probe"))
                    })
                };
                finish_tx.send(()).unwrap();
                let original = first.join().unwrap();
                let joined = waiter.join().unwrap();
                assert_eq!(
                    original.as_ref().map(|v| &v[0].name),
                    joined.as_ref().map(|v| &v[0].name)
                );
            }
        }

        fn pcm(format: &str, rate: u32, channels: u32) -> PcmFormat {
            PcmFormat {
                gst_format: format.into(),
                sample_rate: rate,
                channels,
                bytes_per_sample: match format {
                    "S16LE" => 2,
                    "S24LE" => 3,
                    _ => 4,
                },
            }
        }

        #[test]
        fn strict_format_matrix_distinguishes_integer_depth_float_and_channel_loss() {
            for from in ["S16LE", "S24LE", "S24_32LE", "S32LE", "F32LE", "unknown"] {
                for to in ["S16LE", "S24LE", "S24_32LE", "S32LE", "F32LE", "unknown"] {
                    let expected = match (integer_depth(from), integer_depth(to)) {
                        (Some(f), Some(t)) => t >= f,
                        _ => from == "F32LE" && to == from,
                    };
                    assert_eq!(
                        samples_preserved(&pcm(from, 44100, 2), &pcm(to, 44100, 2)),
                        expected,
                        "{from} -> {to}"
                    );
                }
            }
            assert!(!samples_preserved(
                &pcm("S16LE", 44100, 2),
                &pcm("S32LE", 48000, 2)
            ));
            assert!(!samples_preserved(
                &pcm("S16LE", 44100, 6),
                &pcm("S32LE", 44100, 2)
            ));
        }

        #[test]
        fn blocked_appsink_send_is_cancelled_without_waiting_for_the_writer() {
            let (tx, _rx) = crossbeam_channel::bounded(1);
            tx.send(WriterCommand::Flush).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_cancel = Arc::clone(&cancelled);
            let (started, ready) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                started.send(()).unwrap();
                send_writer_data(
                    &tx,
                    AudioChunk {
                        data: vec![0; 4],
                        format: pcm("S16LE", 44100, 2),
                        generation: 1,
                    },
                    &worker_cancel,
                )
            });
            ready.recv().unwrap();
            cancelled.store(true, Ordering::Release);
            assert_eq!(worker.join().unwrap(), Err(gst::FlowError::Flushing));
        }

        #[cfg(target_os = "linux")]
        struct GuardPipeline {
            pipeline: gst::Pipeline,
            source: gst_app::AppSrc,
            rx: crossbeam_channel::Receiver<WriterCommand>,
            rejected: Arc<AtomicBool>,
            cancelled: Arc<AtomicBool>,
        }

        #[cfg(target_os = "linux")]
        impl GuardPipeline {
            fn new(is_dash: bool, formats: &[&str], channels: u32) -> Self {
                gst::init().unwrap();
                let (tx, rx) = crossbeam_channel::bounded(32);
                let rejected = Arc::new(AtomicBool::new(false));
                let cancelled = Arc::new(AtomicBool::new(false));
                let output = pcm(formats[0], 44100, channels);
                let (pipeline, _, _) = build_appsink_pipeline(AppSinkConfig {
                    uri: "file:///unused-in-synthetic-pcm-test",
                    is_dash,
                    route: crate::proxy::Route::NoProxy,
                    exclusive: true,
                    bit_perfect: true,
                    preserve_rate: false,
                    writer_tx: tx,
                    writer_gen: Arc::new(AtomicU64::new(1)),
                    negotiated_fmt: &output,
                    supported_gst_formats: formats,
                    supported_rates: &[44100, 48000],
                    decoded_cell: Arc::new(Mutex::new(None)),
                    output_cell: Arc::new(Mutex::new(None)),
                    rejected: Arc::clone(&rejected),
                    cancelled: Arc::clone(&cancelled),
                })
                .unwrap();
                // Inject decoded PCM into the real production chain. This exercises
                // its decoder probe, format locking, appsink probe and buffer gate.
                let decoder = pipeline
                    .children()
                    .into_iter()
                    .find(|e| e.factory().is_some_and(|f| f.name() == "uridecodebin"))
                    .unwrap();
                pipeline.remove(&decoder).unwrap();
                let convert = pipeline
                    .children()
                    .into_iter()
                    .find(|e| e.factory().is_some_and(|f| f.name() == "audioconvert"))
                    .unwrap();
                let source = gst_app::AppSrc::builder().format(gst::Format::Time).build();
                pipeline.add(&source).unwrap();
                source.link(&convert).unwrap();
                pipeline.set_state(gst::State::Playing).unwrap();
                Self {
                    pipeline,
                    source,
                    rx,
                    rejected,
                    cancelled,
                }
            }
            fn push(&self, format: &str, rate: i32, channels: i32, bytes: Vec<u8>) {
                self.source.set_caps(Some(
                    &gst::Caps::builder("audio/x-raw")
                        .field("format", format)
                        .field("rate", rate)
                        .field("channels", channels)
                        .field("layout", "interleaved")
                        .build(),
                ));
                let _ = self.source.push_buffer(gst::Buffer::from_mut_slice(bytes));
            }
            fn chunk(&self) -> AudioChunk {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match self
                        .rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(WriterCommand::Data(chunk)) => return chunk,
                        Ok(_) => {}
                        Err(e) => panic!(
                            "No valid PCM: {e}; rejected={}",
                            self.rejected.load(Ordering::Acquire)
                        ),
                    }
                }
            }
            fn assert_rejected(&self) {
                let bus = self.pipeline.bus().unwrap();
                let message = bus
                    .timed_pop_filtered(gst::ClockTime::from_seconds(3), &[gst::MessageType::Error])
                    .expect("actionable format error");
                let gst::MessageView::Error(error) = message.view() else {
                    unreachable!()
                };
                assert!(error.error().to_string().contains("bit-perfect"));
                assert!(self.rejected.load(Ordering::Acquire));
                assert!(self.cancelled.load(Ordering::Acquire));
                assert!(!self
                    .rx
                    .try_iter()
                    .any(|cmd| matches!(cmd, WriterCommand::Data(_))));
            }
        }
        #[cfg(target_os = "linux")]
        impl Drop for GuardPipeline {
            fn drop(&mut self) {
                self.cancelled.store(true, Ordering::Release);
                self.pipeline.set_state(gst::State::Null).unwrap();
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn real_gstreamer_preserves_integer_samples_for_both_transports_and_silent_channels() {
            for is_dash in [false, true] {
                for output_channels in [2, 4] {
                    let chain = GuardPipeline::new(is_dash, &["S32LE"], output_channels);
                    let source = [i16::MIN, -1, 1, i16::MAX];
                    chain.push(
                        "S16LE",
                        44100,
                        2,
                        source.iter().flat_map(|v| v.to_le_bytes()).collect(),
                    );
                    let chunk = chain.chunk();
                    assert_eq!(chunk.format, pcm("S32LE", 44100, output_channels));
                    let actual: Vec<i32> = chunk
                        .data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| i32::from_le_bytes(*b))
                        .collect();
                    let expected: Vec<i32> = source
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .flat_map(|frame| {
                            let mut samples =
                                vec![(frame[0] as i32) << 16, (frame[1] as i32) << 16];
                            samples.resize(output_channels as usize, 0);
                            samples
                        })
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "dash={is_dash}, channels={output_channels}"
                    );
                }
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn real_gstreamer_preserves_24_bit_repacking_and_identical_float_bits() {
            let samples = [-8_388_608i32, -1, 1, 8_388_607];
            let packed: Vec<u8> = samples
                .iter()
                .flat_map(|v| v.to_le_bytes()[..3].to_vec())
                .collect();
            let unpacked: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
            let widened: Vec<u8> = samples
                .iter()
                .flat_map(|v| (v << 8).to_le_bytes())
                .collect();
            let floats: Vec<u8> = [0x8000_0000u32, 0, 0x3f00_0000, 0xbf80_0000]
                .iter()
                .flat_map(|bits| bits.to_le_bytes())
                .collect();
            for is_dash in [false, true] {
                for (source, target, input, expected) in [
                    ("S24LE", "S24_32LE", &packed, &unpacked),
                    ("S24_32LE", "S24LE", &unpacked, &packed),
                    ("S24_32LE", "S32LE", &unpacked, &widened),
                    ("F32LE", "F32LE", &floats, &floats),
                ] {
                    let chain = GuardPipeline::new(is_dash, &[target], 2);
                    chain.push(source, 44100, 2, input.clone());
                    let chunk = chain.chunk();
                    assert_eq!(chunk.format, pcm(target, 44100, 2));
                    assert_eq!(
                        &chunk.data, expected,
                        "{source} -> {target}, dash={is_dash}"
                    );
                }
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn real_gstreamer_rejects_narrowing_and_float_conversion_before_any_writer_buffer() {
            for is_dash in [false, true] {
                for (source, target) in [("S32LE", "S16LE"), ("F32LE", "S32LE"), ("S24LE", "F32LE")]
                {
                    let chain = GuardPipeline::new(is_dash, &[target], 2);
                    chain.push(
                        source,
                        44100,
                        2,
                        vec![0; if source == "S24LE" { 6 } else { 8 }],
                    );
                    chain.assert_rejected();
                }
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn real_gstreamer_revalidates_renegotiation_and_output_rate_at_caps_boundary() {
            for is_dash in [false, true] {
                let chain = GuardPipeline::new(is_dash, &["S16LE"], 2);
                chain.push("S16LE", 44100, 2, vec![0; 4]);
                chain.chunk();
                chain.push("S32LE", 44100, 2, vec![0; 8]);
                chain.assert_rejected();

                let chain = GuardPipeline::new(is_dash, &["S16LE"], 2);
                chain.push("S16LE", 44100, 2, vec![0; 4]);
                chain.chunk();
                let sink = chain
                    .pipeline
                    .children()
                    .into_iter()
                    .find(|e| e.is::<gst_app::AppSink>())
                    .unwrap();
                sink.static_pad("sink")
                    .unwrap()
                    .send_event(gst::event::Caps::new(
                        &gst::Caps::builder("audio/x-raw")
                            .field("format", "S16LE")
                            .field("rate", 48000i32)
                            .field("channels", 2i32)
                            .field("layout", "interleaved")
                            .build(),
                    ));
                chain.assert_rejected();
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn real_gstreamer_rejects_incomplete_output_caps_before_writer_buffers() {
            for is_dash in [false, true] {
                let chain = GuardPipeline::new(is_dash, &["S16LE"], 2);
                chain.push("S16LE", 44100, 2, vec![0; 4]);
                chain.chunk();
                let sink = chain
                    .pipeline
                    .children()
                    .into_iter()
                    .find(|e| e.is::<gst_app::AppSink>())
                    .unwrap();
                sink.static_pad("sink")
                    .unwrap()
                    .send_event(gst::event::Caps::new(
                        &gst::Caps::builder("audio/x-raw")
                            .field("format", "S16LE")
                            .field("layout", "interleaved")
                            .build(),
                    ));
                chain.assert_rejected();
            }
        }
    }

    mod output_transition_tests {
        use super::super::*;

        fn player_with_inbox() -> (AudioPlayer, mpsc::Receiver<AudioCommand>) {
            let (cmd_tx, inbox) = mpsc::channel();
            (
                AudioPlayer {
                    cmd_tx,
                    output_transaction: Arc::new(Mutex::new(())),
                    output_state: Arc::new(Mutex::new(AudioOutputState::new(
                        AudioOutputConfig::default(),
                        Some(AudioOutputConfig::default()),
                    ))),
                    exclusive_device: Arc::new(Mutex::new(None)),
                    decoded_caps_cell: Arc::new(Mutex::new(None)),
                    output_caps_cell: Arc::new(Mutex::new(None)),
                },
                inbox,
            )
        }

        #[test]
        fn settings_commit_never_waits_for_busy_audio_worker() {
            let (player, inbox) = player_with_inbox();
            let wanted = AudioOutputConfig {
                route: AudioOutputRoute::Hqplayer,
                ..Default::default()
            };
            player.configure_output(wanted.clone()).unwrap();
            let state = player.output_state();
            assert_eq!(state.configured, wanted);
            assert_eq!(state.active.unwrap().route, AudioOutputRoute::Native);
            assert!(state.pending);
            assert!(matches!(inbox.try_recv(), Ok(AudioCommand::OutputChanged)));
        }

        #[test]
        fn paused_seek_keeps_active_route_until_explicit_new_play() {
            let native = AudioOutputConfig::default();
            let wanted = AudioOutputConfig {
                route: AudioOutputRoute::Hqplayer,
                ..Default::default()
            };
            let state = AudioOutputState::new(wanted, Some(native.clone()));
            assert_eq!(playback_config(&state, true), native);
            assert_eq!(
                playback_config(&state, false).route,
                AudioOutputRoute::Hqplayer
            );
        }

        #[test]
        fn failed_worker_delivery_rolls_desired_state_back() {
            let (player, inbox) = player_with_inbox();
            drop(inbox);
            let wanted = AudioOutputConfig {
                route: AudioOutputRoute::Hqplayer,
                ..Default::default()
            };
            assert!(player.configure_output(wanted).is_err());
            assert_eq!(
                player.output_state().configured,
                AudioOutputConfig::default()
            );
            assert!(!player.output_state().pending);
        }

        #[test]
        fn stale_start_cannot_publish_after_waiting_for_settings_commit() {
            let gate = Arc::new(Mutex::new(()));
            let generation = Arc::new(AtomicU64::new(4));
            let held = gate.lock().unwrap();
            let other_gate = Arc::clone(&gate);
            let other_generation = Arc::clone(&generation);
            let worker = std::thread::spawn(move || {
                let mut published = false;
                let current = with_current_generation(&other_gate, (&other_generation, 4), || {
                    published = true
                });
                (current, published)
            });
            // A new play claims the generation while the old completion waits.
            generation.fetch_add(1, Ordering::AcqRel);
            drop(held);
            assert_eq!(worker.join().unwrap(), (false, false));
        }

        #[test]
        fn configured_updates_preserve_the_active_playback_instance() {
            let mut state = AudioOutputState::new(
                AudioOutputConfig::default(),
                Some(AudioOutputConfig::default()),
            );
            state.playback_generation = Some(42);
            let active = state.active.clone();
            replace_output_state(
                &mut state,
                AudioOutputConfig {
                    route: AudioOutputRoute::Hqplayer,
                    ..Default::default()
                },
                active,
            );
            assert_eq!(state.playback_generation, Some(42));
            assert!(state.pending);
            let config = state.configured.clone();
            replace_output_state(&mut state, config, None);
            assert_eq!(state.playback_generation, None);
        }

        #[test]
        fn rollback_and_active_updates_never_reuse_revision() {
            let mut state = AudioOutputState::default();
            replace_output_state(&mut state, AudioOutputConfig::default(), None);
            assert_eq!(state.revision, 1);
            replace_output_state(
                &mut state,
                AudioOutputConfig {
                    route: AudioOutputRoute::Hqplayer,
                    ..Default::default()
                },
                None,
            );
            assert_eq!(state.revision, 2);
            replace_output_state(
                &mut state,
                AudioOutputConfig::default(),
                Some(AudioOutputConfig::default()),
            );
            assert_eq!(state.revision, 3);
            assert!(!state.pending);
        }

        #[test]
        fn inactive_preference_changes_do_not_cancel_preload() {
            let (player, inbox) = player_with_inbox();
            let wanted = AudioOutputConfig {
                hqplayer_port: 4322,
                ..Default::default()
            };
            player.configure_output(wanted).unwrap();
            assert!(!player.output_state().pending);
            assert!(inbox.try_recv().is_err());
        }
        fn mock_hq(
            replies: Vec<(&'static str, &'static str)>,
        ) -> (u16, std::thread::JoinHandle<()>) {
            use std::io::{BufRead, BufReader, Write};
            let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = server.local_addr().unwrap().port();
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = server.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut input = BufReader::new(socket.try_clone().unwrap());
                for (expected, response) in replies {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    assert!(line.contains(expected), "unexpected request: {line}");
                    socket.write_all(response.as_bytes()).unwrap();
                }
            });
            (port, worker)
        }

        #[test]
        fn local_output_requires_confirmed_remote_stop() {
            let (port, worker) = mock_hq(vec![
                ("<Stop/>", "<Stop result=\"OK\"/>"),
                (
                    "<Status",
                    "<Status state=\"2\" position=\"10\" length=\"200\"/>",
                ),
                (
                    "<Status",
                    "<Status state=\"0\" position=\"0\" length=\"0\"/>",
                ),
            ]);
            assert!(hq_send_stop(&Mutex::new(None), "127.0.0.1", port).is_ok());
            worker.join().unwrap();
        }

        #[test]
        fn rejected_remote_pause_and_stop_are_not_success() {
            let (port, worker) =
                mock_hq(vec![("<Pause/>", "<Pause result=\"Error\">busy</Pause>")]);
            assert!(
                hq_send_transport(&Mutex::new(None), "127.0.0.1", port, |s| s.pause()).is_err()
            );
            worker.join().unwrap();
            let (port, worker) = mock_hq(vec![("<Stop/>", "<Stop result=\"Error\">busy</Stop>")]);
            assert!(hq_send_stop(&Mutex::new(None), "127.0.0.1", port).is_err());
            worker.join().unwrap();
        }

        #[test]
        fn gapless_promotion_requires_playing_reset_near_previous_end() {
            let previous = crate::hqplayer::Status {
                state: 2,
                position: 199.0,
                length: 200.0,
            };
            let next = crate::hqplayer::Status {
                state: 2,
                position: 0.4,
                length: 200.0,
            };
            assert!(hq_crossed_boundary(previous, next));
            assert!(!hq_crossed_boundary(
                previous,
                crate::hqplayer::Status { state: 1, ..next }
            ));
            assert!(!hq_crossed_boundary(
                crate::hqplayer::Status {
                    position: 20.0,
                    ..previous
                },
                next
            ));
            assert!(!hq_crossed_boundary(
                previous,
                crate::hqplayer::Status {
                    position: 199.5,
                    ..next
                }
            ));
        }

        #[test]
        fn uncommitted_route_snapshot_waits_for_settings_transaction() {
            let (player, _inbox) = player_with_inbox();
            let gate = player.begin_output_update();
            let other = player.clone();
            let (tx, rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let _gate = other.begin_output_update();
                tx.send(other.output_state.lock().unwrap().configured.route)
                    .unwrap();
            });
            player
                .configure_output(AudioOutputConfig {
                    route: AudioOutputRoute::Hqplayer,
                    ..Default::default()
                })
                .unwrap();
            assert!(rx.try_recv().is_err());
            // A failed settings rename rolls back before releasing the gate.
            player
                .configure_output(AudioOutputConfig::default())
                .unwrap();
            drop(gate);
            assert_eq!(rx.recv().unwrap(), AudioOutputRoute::Native);
            worker.join().unwrap();
        }
    }

    mod pcm_gain_tests {
        use super::apply_pcm_gain;

        fn panic_if_dithered() -> f64 {
            panic!("exact gain must not dither");
        }

        #[test]
        fn unity_gain_leaves_every_byte_alone() {
            let mut s16 = vec![0x00, 0x80, 0xFF, 0x7F];
            let before = s16.clone();
            apply_pcm_gain(&mut s16, "S16LE", 1.0, &mut panic_if_dithered);
            assert_eq!(s16, before);

            // High byte is the sign pad. A distinctive pattern must survive.
            let mut s24 = vec![0x11, 0x22, 0x33, 0xFF, 0xFF, 0xFF, 0x80, 0x00];
            let before = s24.clone();
            apply_pcm_gain(&mut s24, "S24_32LE", 1.0, &mut panic_if_dithered);
            assert_eq!(s24, before);
        }

        #[test]
        fn zero_gain_is_digital_silence() {
            let mut s16 = vec![0x34, 0x12, 0xFF, 0x7F];
            apply_pcm_gain(&mut s16, "S16LE", 0.0, &mut panic_if_dithered);
            assert_eq!(s16, vec![0, 0, 0, 0]);

            let mut s24 = vec![0xFF, 0xFF, 0x7F, 0x00];
            apply_pcm_gain(&mut s24, "S24_32LE", 0.0, &mut panic_if_dithered);
            assert_eq!(s24, vec![0, 0, 0, 0]);
        }

        #[test]
        fn an_exact_integer_product_is_not_dithered() {
            // 1000 * 0.5 = 500 exactly.
            let mut data = 1000i16.to_le_bytes().to_vec();
            apply_pcm_gain(&mut data, "S16LE", 0.5, &mut panic_if_dithered);
            assert_eq!(i16::from_le_bytes([data[0], data[1]]), 500);

            let mut neg = (-1000i16).to_le_bytes().to_vec();
            apply_pcm_gain(&mut neg, "S16LE", 0.5, &mut panic_if_dithered);
            assert_eq!(i16::from_le_bytes([neg[0], neg[1]]), -500);
        }

        #[test]
        fn an_inexact_sample_stays_within_one_lsb_of_the_rounded_value() {
            // 1 * 0.4 = 0.4, which rounds to 0. A +0.9 LSB nudge rounds to 1.
            let mut down = 1i16.to_le_bytes().to_vec();
            apply_pcm_gain(&mut down, "S16LE", 0.4, &mut || 0.0);
            assert_eq!(i16::from_le_bytes([down[0], down[1]]), 0);

            let mut up = 1i16.to_le_bytes().to_vec();
            apply_pcm_gain(&mut up, "S16LE", 0.4, &mut || 0.9);
            assert_eq!(i16::from_le_bytes([up[0], up[1]]), 1);
        }

        #[test]
        fn full_scale_attenuation_does_not_overflow() {
            let mut data = i16::MAX.to_le_bytes().to_vec();
            apply_pcm_gain(&mut data, "S16LE", 1.5, &mut || 0.9);
            assert_eq!(i16::from_le_bytes([data[0], data[1]]), i16::MAX);

            let mut wide = i32::MAX.to_le_bytes().to_vec();
            apply_pcm_gain(&mut wide, "S32LE", 1.5, &mut || 0.9);
            assert_eq!(
                i32::from_le_bytes(wide.as_slice().try_into().unwrap()),
                i32::MAX
            );

            // 24-bit full scale must clamp inside the 24-bit range rather than wrap.
            let mut s24 = 8_388_607i32.to_le_bytes().to_vec();
            apply_pcm_gain(&mut s24, "S24_32LE", 1.5, &mut || 0.9);
            assert_eq!(
                i32::from_le_bytes(s24.as_slice().try_into().unwrap()),
                8_388_607
            );
        }

        #[test]
        fn float_samples_are_multiplied_without_dither() {
            let mut data = 0.5f32.to_le_bytes().to_vec();
            apply_pcm_gain(&mut data, "F32LE", 0.5, &mut panic_if_dithered);
            let v = f32::from_le_bytes(data.as_slice().try_into().unwrap());
            assert!((v - 0.25).abs() < 1e-6);
        }
    }
}
