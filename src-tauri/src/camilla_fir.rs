//! In-process CamillaDSP for the exclusive ALSA writer.
//!
//! SONE still opens the playback device. A loaded config's capture and playback
//! devices are ignored for I/O: this module only runs `Pipeline::process_chunk`
//! on PCM the writer already has, then quantizes once back to the device format.
//! The gapless autoaudiosink path never reaches this code.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use camillalib::audiochunk::AudioChunk;
use camillalib::config::{Configuration, OVERRIDES};
use camillalib::pipeline::Pipeline;
use camillalib::ProcessingParameters;

const MAX_CHUNK_FRAMES: usize = 65_536;
const EXACT_EPS: f64 = 1e-4;

/// `load_validate_config` reads the process-global `OVERRIDES` cell. The UI
/// validator and the writer can overlap, so loads take this lock and a drop
/// guard clears the cell even if Camilla panics mid-parse.
static LOAD_LOCK: Mutex<()> = Mutex::new(());

struct OverrideClear;

impl Drop for OverrideClear {
    fn drop(&mut self) {
        let mut overrides = OVERRIDES.write();
        overrides.samplerate = None;
        overrides.channels = None;
        overrides.extra_samples = None;
        overrides.sample_format = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Config channel counts match the exclusive stream.
    All,
    /// Stereo config on a wider device: filter L/R, copy the pad through.
    StereoPad,
    /// Stereo capture expanded by the config to the device channel count.
    StereoExpand,
}

fn decide_route(in_ch: usize, out_ch: usize, stream: usize) -> Result<Route, String> {
    if in_ch == stream && out_ch == stream {
        Ok(Route::All)
    } else if in_ch == 2 && out_ch == 2 && stream > 2 {
        Ok(Route::StereoPad)
    } else if in_ch == 2 && out_ch == stream && stream > 2 {
        Ok(Route::StereoExpand)
    } else {
        Err(format!(
            "CamillaDSP config is {in_ch}→{out_ch} channels, exclusive device is {stream}"
        ))
    }
}

fn bytes_per_sample(gst_format: &str) -> Option<usize> {
    match gst_format {
        "S16LE" => Some(2),
        "S24LE" => Some(3),
        "S24_32LE" | "S32LE" | "F32LE" => Some(4),
        _ => None,
    }
}

/// Integer full-scale is 2^(bits-1), so the negative rail lands on -1.0.
fn full_scale(gst_format: &str) -> Option<f64> {
    match gst_format {
        "S16LE" => Some(32_768.0),
        "S24LE" | "S24_32LE" => Some(8_388_608.0),
        "S32LE" => Some(2_147_483_648.0),
        _ => None,
    }
}

fn load_prepared(path: &str, rate: Option<u32>) -> Result<Configuration, String> {
    let _lock = LOAD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    {
        let mut overrides = OVERRIDES.write();
        overrides.samplerate = rate.map(|value| value as usize);
        overrides.channels = None;
        overrides.extra_samples = None;
        overrides.sample_format = None;
    }
    let _clear = OverrideClear;
    let conf = camillalib::config::load_validate_config(path).map_err(|err| err.to_string())?;
    check_prepared(&conf, rate)?;
    Ok(conf)
}

fn check_prepared(conf: &Configuration, rate: Option<u32>) -> Result<(), String> {
    let chunk = conf.devices.chunksize;
    if chunk == 0 || chunk > MAX_CHUNK_FRAMES {
        return Err(format!(
            "CamillaDSP chunksize {chunk} is outside 1..={MAX_CHUNK_FRAMES}"
        ));
    }
    if let Some(rate) = rate {
        if conf.devices.resampler.is_some() && conf.devices.samplerate != rate as usize {
            return Err(format!(
                "CamillaDSP config resamples to {} Hz, but exclusive output keeps the track rate \
                 {rate} Hz. Remove the resampler or use $samplerate$.",
                conf.devices.samplerate
            ));
        }
        if conf.devices.resampler.is_none() && conf.devices.samplerate != rate as usize {
            return Err(format!(
                "CamillaDSP stayed at {} Hz instead of the track rate {rate} Hz",
                conf.devices.samplerate
            ));
        }
    }
    Ok(())
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return format!("CamillaDSP pipeline failed: {message}");
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return format!("CamillaDSP pipeline failed: {message}");
    }
    "CamillaDSP pipeline failed".to_string()
}

fn build_pipeline(conf: Configuration) -> Result<Pipeline, String> {
    let params = Arc::new(ProcessingParameters::default());
    std::panic::catch_unwind(AssertUnwindSafe(|| Pipeline::from_config(conf, params)))
        .map_err(panic_message)
}

/// Parse the YAML and build the pipeline once, so a missing coefficient file
/// is reported before the path is saved. Playback reloads at the track rate.
pub(crate) fn validate_config_file(path: &str) -> Result<(), String> {
    let conf = load_prepared(path, None)?;
    let _pipeline = build_pipeline(conf)?;
    Ok(())
}

fn decode_interleaved(data: &[u8], gst_format: &str, channels: usize) -> Result<Vec<f64>, String> {
    let bps = bytes_per_sample(gst_format)
        .ok_or_else(|| format!("unsupported PCM format {gst_format}"))?;
    let frame = bps * channels;
    if frame == 0 {
        return Err("PCM frame size is zero".into());
    }
    let usable = data.len() - data.len() % frame;
    let mut out = Vec::with_capacity(usable / bps);
    match gst_format {
        "S16LE" => {
            for chunk in data[..usable].chunks_exact(2) {
                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                out.push(sample as f64 / 32_768.0);
            }
        }
        "S24LE" => {
            for chunk in data[..usable].chunks_exact(3) {
                let raw =
                    chunk[0] as i32 | ((chunk[1] as i32) << 8) | ((chunk[2] as i8 as i32) << 16);
                out.push(raw as f64 / 8_388_608.0);
            }
        }
        "S24_32LE" => {
            for chunk in data[..usable].chunks_exact(4) {
                let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(sample as f64 / 8_388_608.0);
            }
        }
        "S32LE" => {
            for chunk in data[..usable].chunks_exact(4) {
                let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(sample as f64 / 2_147_483_648.0);
            }
        }
        "F32LE" => {
            for chunk in data[..usable].chunks_exact(4) {
                let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out.push(sample as f64);
            }
        }
        other => return Err(format!("unsupported PCM format {other}")),
    }
    Ok(out)
}

fn quantize(scaled: f64, lo: i64, hi: i64, dither: &mut impl FnMut() -> f64) -> i64 {
    let finite = if scaled.is_finite() { scaled } else { 0.0 };
    let nearest = finite.round();
    let quant = if (finite - nearest).abs() < EXACT_EPS {
        nearest
    } else {
        (finite + dither()).round()
    };
    quant.clamp(lo as f64, hi as f64) as i64
}

fn encode_planar(
    planar: &[Vec<f64>],
    frames: usize,
    gst_format: &str,
    gain: f64,
    dither: &mut impl FnMut() -> f64,
) -> Result<Vec<u8>, String> {
    let channels = planar.len();
    let bps = bytes_per_sample(gst_format)
        .ok_or_else(|| format!("unsupported PCM format {gst_format}"))?;
    let mut out = vec![0u8; frames * channels * bps];
    #[allow(clippy::float_cmp)]
    if gain == 0.0 {
        return Ok(out);
    }

    if gst_format == "F32LE" {
        for (channel, wave) in planar.iter().enumerate() {
            for (frame, &sample) in wave.iter().take(frames).enumerate() {
                let sample = if sample.is_finite() { sample } else { 0.0 };
                let value = ((sample * gain) as f32).clamp(-1.0, 1.0);
                let offset = (frame * channels + channel) * 4;
                out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        return Ok(out);
    }

    let scale =
        full_scale(gst_format).ok_or_else(|| format!("unsupported PCM format {gst_format}"))?;
    let (lo, hi) = match gst_format {
        "S16LE" => (i16::MIN as i64, i16::MAX as i64),
        "S24LE" | "S24_32LE" => (-8_388_608, 8_388_607),
        "S32LE" => (i32::MIN as i64, i32::MAX as i64),
        other => return Err(format!("unsupported PCM format {other}")),
    };

    for (channel, wave) in planar.iter().enumerate() {
        for (frame, &sample) in wave.iter().take(frames).enumerate() {
            let sample = if sample.is_finite() { sample } else { 0.0 };
            let value = quantize(sample * gain * scale, lo, hi, dither);
            let offset = (frame * channels + channel) * bps;
            match gst_format {
                "S16LE" => {
                    out[offset..offset + 2].copy_from_slice(&(value as i16).to_le_bytes());
                }
                "S32LE" | "S24_32LE" => {
                    out[offset..offset + 4].copy_from_slice(&(value as i32).to_le_bytes());
                }
                "S24LE" => {
                    out[offset] = value as u8;
                    out[offset + 1] = (value >> 8) as u8;
                    out[offset + 2] = (value >> 16) as u8;
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

struct Prepared {
    path: String,
    rate: u32,
    conf: Configuration,
}

struct FirEngine {
    pipeline: Pipeline,
    chunk_frames: usize,
    route: Route,
    out_channels: usize,
    stream_channels: usize,
    rate: u32,
}

impl FirEngine {
    fn process(
        &mut self,
        interleaved: &[f64],
        gst_format: &str,
        gain: f64,
        dither: &mut impl FnMut() -> f64,
    ) -> Result<Vec<u8>, String> {
        let frames = self.chunk_frames;
        let stream = self.stream_channels;
        if interleaved.len() != frames * stream {
            return Err("CamillaDSP chunk length does not match the stream".into());
        }
        let feed = match self.route {
            Route::All => stream,
            Route::StereoPad | Route::StereoExpand => 2,
        };
        let mut waveforms = vec![vec![0.0; frames]; feed];
        for frame in 0..frames {
            for channel in 0..feed {
                waveforms[channel][frame] = interleaved[frame * stream + channel];
            }
        }
        let chunk = AudioChunk::new(waveforms, 1.0, -1.0, frames, frames);
        let processed =
            std::panic::catch_unwind(AssertUnwindSafe(|| self.pipeline.process_chunk(chunk)))
                .map_err(panic_message)?;
        if processed.waveforms.len() != self.out_channels {
            return Err(format!(
                "CamillaDSP returned {} channels, config playback is {}",
                processed.waveforms.len(),
                self.out_channels
            ));
        }
        if processed.waveforms.iter().any(|wave| wave.len() != frames) {
            return Err("CamillaDSP returned a short chunk".into());
        }

        let mut planar = vec![vec![0.0; frames]; stream];
        match self.route {
            Route::All | Route::StereoExpand => {
                for (channel, wave) in processed.waveforms.iter().enumerate().take(stream) {
                    planar[channel].copy_from_slice(wave);
                }
            }
            Route::StereoPad => {
                planar[0].copy_from_slice(&processed.waveforms[0]);
                planar[1].copy_from_slice(&processed.waveforms[1]);
                for frame in 0..frames {
                    for channel in 2..stream {
                        planar[channel][frame] = interleaved[frame * stream + channel];
                    }
                }
            }
        }
        encode_planar(&planar, frames, gst_format, gain, dither)
    }
}

/// What the writer should do with the buffer it just pulled.
///
/// `Off` means FIR is not in the path: the caller runs the normal gain stage
/// on the original bytes. `Processed` means those bytes were consumed, even
/// when the vec is empty because a Camilla chunk is not full yet. Writing the
/// original in that case would play the partial twice.
#[derive(Debug)]
pub(crate) enum FirOutput {
    Off,
    Processed(Vec<u8>),
    /// First failure for this path, rate, and channel count. Later chunks of
    /// the same key return `Off` so playback continues as direct PCM.
    Failed(String),
}

pub(crate) struct FirSlot {
    path: Option<String>,
    generation: u64,
    engine: Option<FirEngine>,
    prepared: Option<Prepared>,
    /// Interleaved f64 at the stream channel count. `pending_read` drops the
    /// consumed prefix without shifting the vec on every chunk.
    pending: Vec<f64>,
    pending_read: usize,
    pending_channels: usize,
    pending_rate: u32,
    dirty: bool,
    failed_key: Option<String>,
    /// Set when this call already produced filtered bytes and then failed.
    /// `Failed` cannot carry those bytes: the writer would also play the
    /// original buffer. The writer emits the bypass notice and drops the
    /// original instead.
    bypass_message: Option<String>,
}

impl FirSlot {
    pub(crate) fn new() -> Self {
        Self {
            path: None,
            generation: 0,
            engine: None,
            prepared: None,
            pending: Vec::new(),
            pending_read: 0,
            pending_channels: 0,
            pending_rate: 0,
            dirty: false,
            failed_key: None,
            bypass_message: None,
        }
    }

    /// Same path and generation is the per-track arming call and must keep the
    /// live engine. A new generation reloads the YAML, which is how choosing
    /// the same file again picks up an edit on the next track.
    pub(crate) fn set_path(&mut self, path: Option<String>, generation: u64) -> bool {
        if self.generation == generation && self.path == path {
            return false;
        }
        self.path = path;
        self.generation = generation;
        self.engine = None;
        self.prepared = None;
        self.pending.clear();
        self.pending_read = 0;
        self.dirty = true;
        self.failed_key = None;
        self.bypass_message = None;
        true
    }

    pub(crate) fn take_bypass_message(&mut self) -> Option<String> {
        self.bypass_message.take()
    }

    /// Drop samples that belong to the previous seek or track. The pipeline is
    /// rebuilt on the next render so overlap state does not cross the boundary,
    /// and so a flush does not wait on an FFT plan while ALSA is dropping.
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
        self.pending_read = 0;
        self.dirty = true;
    }

    pub(crate) fn render(
        &mut self,
        data: &[u8],
        sample_rate: u32,
        channels: u32,
        gst_format: &str,
        gain: f32,
        dither: &mut impl FnMut() -> f64,
    ) -> FirOutput {
        let Some(path) = self.path.clone() else {
            return FirOutput::Off;
        };
        if sample_rate == 0 || channels == 0 {
            return FirOutput::Failed("exclusive device format is incomplete".into());
        }
        let key = format!("{path}|{sample_rate}|{channels}");
        if self.failed_key.as_deref() == Some(key.as_str()) {
            return FirOutput::Off;
        }
        let gain = if gain.is_finite() {
            f64::from(gain)
        } else {
            1.0
        };
        let decoded = match decode_interleaved(data, gst_format, channels as usize) {
            Ok(samples) => samples,
            Err(err) => return self.fail(key, err),
        };
        self.append_samples(&decoded, channels as usize, sample_rate);
        if let Err(err) = self.ensure_engine(&path, sample_rate, channels as usize) {
            return self.fail(key, err);
        }
        match self.drain_full_chunks(gst_format, gain, dither) {
            Ok(bytes) => FirOutput::Processed(bytes),
            Err((produced, err)) => self.finish_partial(key, produced, err),
        }
    }

    /// Pad one partial chunk with zeros so the last frames of a track are not
    /// dropped. Emits nothing when no frames are waiting. Does not play out a
    /// longer FIR tail: the next track's flush would discard it.
    pub(crate) fn flush(
        &mut self,
        sample_rate: u32,
        channels: u32,
        gst_format: &str,
        gain: f32,
        dither: &mut impl FnMut() -> f64,
    ) -> FirOutput {
        let Some(path) = self.path.clone() else {
            return FirOutput::Off;
        };
        if self.available_frames() == 0 {
            return FirOutput::Processed(Vec::new());
        }
        if self.pending_rate != sample_rate || self.pending_channels != channels as usize {
            self.pending.clear();
            self.pending_read = 0;
            return FirOutput::Processed(Vec::new());
        }
        let key = format!("{path}|{sample_rate}|{channels}");
        if self.failed_key.as_deref() == Some(key.as_str()) {
            return FirOutput::Off;
        }
        let gain = if gain.is_finite() {
            f64::from(gain)
        } else {
            1.0
        };
        if let Err(err) = self.ensure_engine(&path, sample_rate, channels as usize) {
            return self.fail(key, err);
        }
        let chunk_frames = match self.engine.as_ref() {
            Some(engine) => engine.chunk_frames,
            None => return self.fail(key, "CamillaDSP engine is not loaded".into()),
        };
        let mut produced = match self.drain_full_chunks(gst_format, gain, dither) {
            Ok(bytes) => bytes,
            Err((produced, err)) => return self.finish_partial(key, produced, err),
        };
        let leftover = self.available_frames();
        if leftover == 0 {
            return FirOutput::Processed(produced);
        }
        if leftover > chunk_frames {
            return self.fail(key, "CamillaDSP flush held more than one chunk".into());
        }
        let pad = chunk_frames - leftover;
        self.pending
            .extend(std::iter::repeat_n(0.0, pad * self.pending_channels));
        match self.drain_full_chunks(gst_format, gain, dither) {
            Ok(bytes) => {
                produced.extend(bytes);
                FirOutput::Processed(produced)
            }
            Err((extra, err)) => {
                produced.extend(extra);
                self.finish_partial(key, produced, err)
            }
        }
    }

    /// A later block failed after earlier blocks in this call already
    /// produced PCM. Keep those bytes, remember the key, and hand the
    /// message to the writer. An empty failure is a normal bypass.
    fn finish_partial(&mut self, key: String, produced: Vec<u8>, err: String) -> FirOutput {
        if produced.is_empty() {
            self.fail(key, err)
        } else {
            self.bypass_message = Some(err);
            self.mark_failed(key);
            FirOutput::Processed(produced)
        }
    }

    fn mark_failed(&mut self, key: String) {
        self.failed_key = Some(key);
        self.engine = None;
        self.pending.clear();
        self.pending_read = 0;
        self.dirty = true;
    }

    fn fail(&mut self, key: String, err: String) -> FirOutput {
        self.bypass_message = None;
        self.mark_failed(key);
        FirOutput::Failed(err)
    }

    fn available_frames(&self) -> usize {
        if self.pending_channels == 0 {
            return 0;
        }
        self.pending.len().saturating_sub(self.pending_read) / self.pending_channels
    }

    fn append_samples(&mut self, samples: &[f64], channels: usize, rate: u32) {
        if self.pending_channels != channels || self.pending_rate != rate {
            self.pending.clear();
            self.pending_read = 0;
            self.pending_channels = channels;
            self.pending_rate = rate;
            if self
                .engine
                .as_ref()
                .is_some_and(|engine| engine.rate != rate || engine.stream_channels != channels)
            {
                self.dirty = true;
            }
        }
        if self.pending_read > 0 && self.pending_read == self.pending.len() {
            self.pending.clear();
            self.pending_read = 0;
        }
        self.pending.extend_from_slice(samples);
    }

    fn take_interleaved(&mut self, frames: usize) -> Vec<f64> {
        let count = frames * self.pending_channels;
        let start = self.pending_read;
        let end = start + count;
        let block = self.pending[start..end].to_vec();
        self.pending_read = end;
        if self.pending_read >= 4096 && self.pending_read * 2 >= self.pending.len() {
            self.pending.drain(..self.pending_read);
            self.pending_read = 0;
        }
        block
    }

    fn prepared_conf(&mut self, path: &str, rate: u32) -> Result<Configuration, String> {
        if let Some(prepared) = &self.prepared {
            if prepared.path == path && prepared.rate == rate {
                return Ok(prepared.conf.clone());
            }
        }
        let conf = load_prepared(path, Some(rate))?;
        self.prepared = Some(Prepared {
            path: path.to_string(),
            rate,
            conf: conf.clone(),
        });
        Ok(conf)
    }

    fn ensure_engine(&mut self, path: &str, rate: u32, channels: usize) -> Result<(), String> {
        let rebuild = match &self.engine {
            None => true,
            Some(engine) => self.dirty || engine.rate != rate || engine.stream_channels != channels,
        };
        if !rebuild {
            return Ok(());
        }
        let conf = self.prepared_conf(path, rate)?;
        let in_ch = conf.devices.capture.channels();
        let out_ch = conf.devices.playback.channels();
        let route = decide_route(in_ch, out_ch, channels)?;
        let chunk_frames = conf.devices.chunksize;
        let pipeline = build_pipeline(conf)?;
        log::info!(
            "[camilla-fir] pipeline ready at {rate} Hz, chunk {chunk_frames}, {}→{} on {channels} ch",
            in_ch,
            out_ch
        );
        self.engine = Some(FirEngine {
            pipeline,
            chunk_frames,
            route,
            out_channels: out_ch,
            stream_channels: channels,
            rate,
        });
        self.dirty = false;
        Ok(())
    }

    fn drain_full_chunks(
        &mut self,
        gst_format: &str,
        gain: f64,
        dither: &mut impl FnMut() -> f64,
    ) -> Result<Vec<u8>, (Vec<u8>, String)> {
        let mut produced = Vec::new();
        loop {
            let (chunk_frames, channels) = match self.engine.as_ref() {
                Some(engine) => (engine.chunk_frames, engine.stream_channels),
                None => {
                    return Err((produced, "CamillaDSP engine is not loaded".into()));
                }
            };
            if self.pending_channels != channels {
                return Err((
                    produced,
                    "CamillaDSP channel count changed inside a chunk".into(),
                ));
            }
            if self.available_frames() < chunk_frames {
                break;
            }
            let block = self.take_interleaved(chunk_frames);
            let Some(engine) = self.engine.as_mut() else {
                return Err((produced, "CamillaDSP engine is not loaded".into()));
            };
            match engine.process(&block, gst_format, gain, dither) {
                Ok(bytes) => produced.extend(bytes),
                Err(err) => return Err((produced, err)),
            }
        }
        Ok(produced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_yaml(
        dir: &std::path::Path,
        capture: usize,
        playback: usize,
        extra: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(format!("fir-{capture}-{playback}.yml"));
        let yaml = format!(
            "\
devices:
  samplerate: 48000
  chunksize: 8
  capture:
    type: Stdin
    channels: {capture}
    format: F64_LE
  playback:
    type: Stdout
    channels: {playback}
    format: F64_LE
{extra}filters:
  gain:
    type: Gain
    parameters:
      gain: 0.0
pipeline:
  - type: Filter
    channels: [0, 1]
    names:
      - gain
"
        );
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn s16_bytes(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    fn s16_samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect()
    }

    fn processed(out: FirOutput) -> Vec<u8> {
        match out {
            FirOutput::Processed(bytes) => bytes,
            other => panic!("expected processed PCM, got {other:?}"),
        }
    }

    fn stereo(frames: usize, left: i16, right: i16) -> Vec<i16> {
        let mut samples = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            samples.push(left);
            samples.push(right);
        }
        samples
    }

    #[test]
    fn missing_and_invalid_configs_fail_validation() {
        let err = validate_config_file("/no/such/camilla-fir.yml").unwrap_err();
        assert!(
            err.contains("Could not open") || err.contains("camilla"),
            "{err}"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yml");
        std::fs::write(&path, "this: is not a camilladsp config\n").unwrap();
        assert!(validate_config_file(path.to_str().unwrap()).is_err());

        let zero = dir.path().join("zero.yml");
        std::fs::write(
            &zero,
            "\
devices:
  samplerate: 48000
  chunksize: 0
  capture:
    type: Stdin
    channels: 2
    format: F64_LE
  playback:
    type: Stdout
    channels: 2
    format: F64_LE
filters:
  gain:
    type: Gain
    parameters:
      gain: 0.0
pipeline:
  - type: Filter
    channels: [0, 1]
    names:
      - gain
",
        )
        .unwrap();
        let err = validate_config_file(zero.to_str().unwrap()).unwrap_err();
        assert!(err.contains("chunksize"), "{err}");
    }

    #[test]
    fn gain_yaml_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        validate_config_file(path.to_str().unwrap()).unwrap();
    }

    #[test]
    fn unity_s16_is_bit_exact_and_holds_a_partial_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut slot = FirSlot::new();
        assert!(matches!(
            slot.render(&[], 48_000, 2, "S16LE", 1.0, &mut || panic!("dither")),
            FirOutput::Off
        ));
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);

        let mut dither = || panic!("unity gain must not dither");
        let partial = slot.render(
            &s16_bytes(&stereo(7, 16_384, -16_384)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut dither,
        );
        assert!(processed(partial).is_empty());

        let full = slot.render(
            &s16_bytes(&stereo(1, 16_384, -16_384)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut dither,
        );
        let samples = s16_samples(&processed(full));
        assert_eq!(samples, stereo(8, 16_384, -16_384));
    }

    #[test]
    fn flush_pads_one_chunk_and_discard_drops_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut slot = FirSlot::new();
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);
        let mut dither = || panic!("unity gain must not dither");
        assert!(processed(slot.render(
            &s16_bytes(&stereo(3, 16_384, -16_384)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut dither,
        ))
        .is_empty());

        let flushed = processed(slot.flush(48_000, 2, "S16LE", 1.0, &mut dither));
        let samples = s16_samples(&flushed);
        assert_eq!(samples.len(), 16);
        assert_eq!(&samples[..6], &stereo(3, 16_384, -16_384)[..]);
        assert!(samples[6..].iter().all(|sample| *sample == 0));

        assert!(processed(slot.flush(48_000, 2, "S16LE", 1.0, &mut dither)).is_empty());

        let _ = slot.render(
            &s16_bytes(&stereo(3, 16_384, -16_384)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut dither,
        );
        slot.discard();
        let clean = processed(slot.render(
            &s16_bytes(&stereo(8, 1_000, -1_000)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut dither,
        ));
        assert_eq!(s16_samples(&clean), stereo(8, 1_000, -1_000));
    }

    #[test]
    fn silence_gain_consumes_a_full_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut slot = FirSlot::new();
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);
        let out = processed(slot.render(
            &s16_bytes(&stereo(8, 16_384, -16_384)),
            48_000,
            2,
            "S16LE",
            0.0,
            &mut || panic!("silence must not dither"),
        ));
        assert_eq!(out.len(), 32);
        assert!(out.iter().all(|byte| *byte == 0));
        assert_eq!(slot.available_frames(), 0);
    }

    #[test]
    fn mono_stream_rejects_a_stereo_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut slot = FirSlot::new();
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);
        match slot.render(&s16_bytes(&[16_384]), 48_000, 1, "S16LE", 1.0, &mut || 0.0) {
            FirOutput::Failed(message) => {
                assert!(message.contains("2→2"), "{message}");
                assert!(message.contains('1'), "{message}");
            }
            other => panic!("expected a channel rejection, got {other:?}"),
        }
    }

    #[test]
    fn rate_override_scales_chunksize() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut fast = FirSlot::new();
        fast.set_path(Some(path.to_string_lossy().into_owned()), 1);
        assert!(processed(fast.render(
            &s16_bytes(&stereo(15, 0, 0)),
            96_000,
            2,
            "S16LE",
            1.0,
            &mut || 0.0,
        ))
        .is_empty());
        assert_eq!(fast.engine.as_ref().unwrap().chunk_frames, 16);
        assert_eq!(
            processed(fast.render(
                &s16_bytes(&stereo(1, 0, 0)),
                96_000,
                2,
                "S16LE",
                1.0,
                &mut || 0.0
            ))
            .len(),
            16 * 2 * 2
        );

        let mut slow = FirSlot::new();
        slow.set_path(Some(path.to_string_lossy().into_owned()), 1);
        let _ = slow.render(
            &s16_bytes(&stereo(1, 0, 0)),
            44_100,
            2,
            "S16LE",
            1.0,
            &mut || 0.0,
        );
        assert_eq!(slow.engine.as_ref().unwrap().chunk_frames, 8);
    }

    #[test]
    fn resampler_is_rejected_when_the_track_rate_differs() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "  resampler:\n    type: Synchronous\n");
        let mut slot = FirSlot::new();
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);
        match slot.render(
            &s16_bytes(&stereo(1, 0, 0)),
            96_000,
            2,
            "S16LE",
            1.0,
            &mut || 0.0,
        ) {
            FirOutput::Failed(message) => assert!(message.contains("$samplerate$"), "{message}"),
            other => panic!("expected a resampler rejection, got {other:?}"),
        }
    }

    #[test]
    fn stereo_pad_filters_the_first_pair_and_copies_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "");
        let mut slot = FirSlot::new();
        slot.set_path(Some(path.to_string_lossy().into_owned()), 1);
        let mut samples = Vec::new();
        for _ in 0..8 {
            samples.extend_from_slice(&[16_384, -16_384, 1_000, -1_000]);
        }
        let mut dither = || panic!("pad passthrough must not dither");
        let out =
            processed(slot.render(&s16_bytes(&samples), 48_000, 4, "S16LE", 1.0, &mut dither));
        assert_eq!(s16_samples(&out), samples);
    }

    #[test]
    fn same_generation_keeps_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let path = gain_yaml(dir.path(), 2, 2, "")
            .to_string_lossy()
            .into_owned();
        let mut slot = FirSlot::new();
        assert!(slot.set_path(Some(path.clone()), 1));
        let _ = slot.render(
            &s16_bytes(&stereo(1, 1, -1)),
            48_000,
            2,
            "S16LE",
            1.0,
            &mut || 0.0,
        );
        assert!(slot.engine.is_some());
        assert!(!slot.set_path(Some(path.clone()), 1));
        assert!(slot.engine.is_some());
        assert!(!slot.dirty);
        assert!(slot.set_path(Some(path), 2));
        assert!(slot.engine.is_none());
    }
}
