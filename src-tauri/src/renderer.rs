//! Process-local WebKit experiments; run before GTK starts any threads.

const DMABUF_ENV: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererProfile {
    Auto,
    Dmabuf,
    Compatibility,
}

impl RendererProfile {
    fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            None | Some("auto") => Some(Self::Auto),
            Some("dmabuf") => Some(Self::Dmabuf),
            Some("compatibility") => Some(Self::Compatibility),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dmabuf => "dmabuf",
            Self::Compatibility => "compatibility",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RendererPolicy {
    disable_dmabuf: Option<&'static str>,
    reason: &'static str,
}

/// Presence, including an empty or non-UTF-8 value, is an explicit override.
/// Let WebKit select its renderer automatically: a loaded NVIDIA module does
/// not identify the GPU WebKit uses, especially on hybrid systems.
fn renderer_policy(profile: RendererProfile, explicit_dmabuf_override: bool) -> RendererPolicy {
    let (disable_dmabuf, reason) = if explicit_dmabuf_override {
        (None, "explicit-webkit-override")
    } else {
        match profile {
            RendererProfile::Auto => (None, "webkit-default"),
            RendererProfile::Dmabuf => (Some("0"), "dmabuf-allowed"),
            RendererProfile::Compatibility => (Some("1"), "dmabuf-disabled"),
        }
    };
    RendererPolicy {
        disable_dmabuf,
        reason,
    }
}

fn nvidia_module_loaded(modules: &str) -> bool {
    modules.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|name| name == "nvidia" || name.starts_with("nvidia_"))
    })
}

pub struct RendererLaunch {
    pub disable_dmabuf: Option<&'static str>,
    profile: RendererProfile,
    reason: &'static str,
    nvidia_loaded: bool,
}

/// Read the launch environment; only main may apply the returned mutation.
pub fn renderer_launch() -> RendererLaunch {
    let requested = std::env::var_os("SONE_RENDERER");
    let requested_text = requested.as_deref().map(|value| value.to_string_lossy());
    let profile = RendererProfile::parse(requested_text.as_deref()).unwrap_or_else(|| {
        eprintln!(
            "[sone] Unrecognized SONE_RENDERER={requested:?}; using auto. \
             Expected auto, dmabuf, or compatibility."
        );
        RendererProfile::Auto
    });
    let nvidia_loaded = std::fs::read_to_string("/proc/modules")
        .map(|modules| nvidia_module_loaded(&modules))
        .unwrap_or(false);
    let policy = renderer_policy(profile, std::env::var_os(DMABUF_ENV).is_some());

    RendererLaunch {
        disable_dmabuf: policy.disable_dmabuf,
        profile,
        reason: policy.reason,
        nvidia_loaded,
    }
}

impl RendererLaunch {
    /// Called after main has applied the launch policy.
    pub fn log_env(&self) {
        // Log the launch policy, not a claim about the actual GPU or transport.
        // Existing compositing and shared-memory overrides remain untouched.
        eprintln!(
            "[sone] renderer profile={} decision={} nvidia_module_detected={}; \
         WEBKIT_DISABLE_DMABUF_RENDERER={:?} \
         WEBKIT_DISABLE_COMPOSITING_MODE={:?} \
         WEBKIT_FORCE_COMPOSITING_MODE={:?} \
         WEBKIT_DMABUF_RENDERER_FORCE_SHM={:?}",
            self.profile.name(),
            self.reason,
            self.nvidia_loaded,
            std::env::var_os(DMABUF_ENV),
            std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE"),
            std::env::var_os("WEBKIT_FORCE_COMPOSITING_MODE"),
            std::env::var_os("WEBKIT_DMABUF_RENDERER_FORCE_SHM"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_defers_to_webkit_without_a_gpu_module_policy() {
        assert_eq!(RendererProfile::parse(None), Some(RendererProfile::Auto));
        assert_eq!(
            renderer_policy(RendererProfile::Auto, false),
            RendererPolicy {
                disable_dmabuf: None,
                reason: "webkit-default",
            }
        );
    }

    #[test]
    fn experiments_apply_only_the_requested_renderer_override() {
        assert_eq!(
            renderer_policy(RendererProfile::Dmabuf, false).disable_dmabuf,
            Some("0")
        );
        assert_eq!(
            renderer_policy(RendererProfile::Compatibility, false).disable_dmabuf,
            Some("1")
        );
    }

    #[test]
    fn native_webkit_override_wins_over_every_profile() {
        for profile in [
            RendererProfile::Auto,
            RendererProfile::Dmabuf,
            RendererProfile::Compatibility,
        ] {
            assert_eq!(
                renderer_policy(profile, true),
                RendererPolicy {
                    disable_dmabuf: None,
                    reason: "explicit-webkit-override",
                }
            );
        }
    }

    #[test]
    fn profiles_are_explicit_and_invalid_values_are_rejected() {
        for profile in [
            RendererProfile::Auto,
            RendererProfile::Dmabuf,
            RendererProfile::Compatibility,
        ] {
            assert_eq!(RendererProfile::parse(Some(profile.name())), Some(profile));
        }
        for invalid in ["", "automatic", "software", "DMABUF"] {
            assert_eq!(RendererProfile::parse(Some(invalid)), None);
        }
    }

    #[test]
    fn module_detection_matches_names_only() {
        assert!(nvidia_module_loaded("nvidia 123 0 - Live 0\n"));
        assert!(nvidia_module_loaded("nvidia_drm 123 1 - Live 0\n"));
        assert!(!nvidia_module_loaded("i915 123 1 nvidia, Live 0\n"));
        assert!(!nvidia_module_loaded("nvidiaish 123 0 - Live 0\n"));
        assert!(!nvidia_module_loaded("\n"));
    }
}
