//! Remembered output preferences are distinct from the active track's route.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioOutputRoute {
    #[default]
    Native,
    Camilla,
    Hqplayer,
}

pub fn default_hqplayer_host() -> String {
    "127.0.0.1".into()
}

pub fn default_hqplayer_port() -> u16 {
    4321
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputConfig {
    pub route: AudioOutputRoute,
    pub exclusive_mode: bool,
    pub bit_perfect: bool,
    pub device: Option<String>,
    pub camilla_config: Option<String>,
    pub hqplayer_host: String,
    pub hqplayer_port: u16,
}

impl Default for AudioOutputConfig {
    fn default() -> Self {
        Self {
            route: AudioOutputRoute::Native,
            exclusive_mode: false,
            bit_perfect: false,
            device: None,
            camilla_config: None,
            hqplayer_host: default_hqplayer_host(),
            hqplayer_port: default_hqplayer_port(),
        }
    }
}

impl AudioOutputConfig {
    pub fn from_settings(settings: &crate::Settings) -> Self {
        // The experimental build allowed inactive Camilla preferences alongside
        // ordinary system output. Preserve what would actually have played.
        let route = settings.output_route.unwrap_or_else(|| {
            if settings.hqplayer {
                AudioOutputRoute::Hqplayer
            } else if settings.camilla_fir
                && (settings.exclusive_mode || settings.bit_perfect)
                && settings
                    .camilla_config
                    .as_deref()
                    .is_some_and(|path| !path.trim().is_empty())
            {
                AudioOutputRoute::Camilla
            } else {
                AudioOutputRoute::Native
            }
        });
        Self {
            route,
            exclusive_mode: settings.exclusive_mode || settings.bit_perfect,
            bit_perfect: settings.bit_perfect,
            device: settings.exclusive_device.clone(),
            camilla_config: settings.camilla_config.clone(),
            hqplayer_host: if settings.hqplayer_host.trim().is_empty() {
                default_hqplayer_host()
            } else {
                settings.hqplayer_host.trim().to_owned()
            },
            hqplayer_port: if settings.hqplayer_port == 0 {
                default_hqplayer_port()
            } else {
                settings.hqplayer_port
            },
        }
    }

    pub fn write_settings(&self, settings: &mut crate::Settings) {
        settings.output_route = Some(self.route);
        settings.exclusive_mode = self.exclusive_mode || self.bit_perfect;
        settings.bit_perfect = self.bit_perfect;
        settings.exclusive_device = self.device.clone();
        settings.camilla_config = self.camilla_config.clone();
        settings.hqplayer_host = self.hqplayer_host.clone();
        settings.hqplayer_port = self.hqplayer_port;
        settings.camilla_fir = self.route == AudioOutputRoute::Camilla;
        settings.hqplayer = self.route == AudioOutputRoute::Hqplayer;
    }

    pub fn validate(&mut self) -> Result<(), String> {
        self.exclusive_mode |= self.bit_perfect;
        self.camilla_config = self
            .camilla_config
            .take()
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty());
        if self.route == AudioOutputRoute::Camilla && self.camilla_config.is_none() {
            return Err("Choose a CamillaDSP YAML configuration before enabling DSP".into());
        }
        self.hqplayer_host = self.hqplayer_host.trim().to_owned();
        if self.hqplayer_host.is_empty() || self.hqplayer_host.eq_ignore_ascii_case("localhost") {
            self.hqplayer_host = default_hqplayer_host();
        }
        // An inactive old LAN preference may be retained, but cannot be enabled
        // or silently redirected to a different HQPlayer instance.
        if self.route == AudioOutputRoute::Hqplayer {
            if !matches!(self.hqplayer_host.as_str(), "127.0.0.1" | "::1") {
                return Err(
                    "HQPlayer currently supports local Desktop only; choose 127.0.0.1".into(),
                );
            }
            if self.hqplayer_port == 0 {
                return Err("HQPlayer control port must be between 1 and 65535".into());
            }
        }
        Ok(())
    }

    /// Flags describing this route once it starts, without destroying native
    /// preferences in the configured copy.
    pub fn effective(&self) -> Self {
        let mut active = self.clone();
        match active.route {
            AudioOutputRoute::Native => active.exclusive_mode |= active.bit_perfect,
            AudioOutputRoute::Camilla => {
                active.exclusive_mode = true;
                active.bit_perfect = false;
            }
            AudioOutputRoute::Hqplayer => {
                active.exclusive_mode = false;
                active.bit_perfect = false;
                active.device = None;
            }
        }
        active
    }

    /// Remembered inactive options do not make an unchanged track "pending".
    pub fn same_processing_as(&self, other: &Self) -> bool {
        if self.route != other.route {
            return false;
        }
        match self.route {
            AudioOutputRoute::Native => {
                let a = self.effective();
                let b = other.effective();
                a.exclusive_mode == b.exclusive_mode
                    && a.bit_perfect == b.bit_perfect
                    && (!a.exclusive_mode || a.device == b.device)
            }
            AudioOutputRoute::Camilla => {
                self.device == other.device && self.camilla_config == other.camilla_config
            }
            AudioOutputRoute::Hqplayer => {
                self.hqplayer_host == other.hqplayer_host
                    && self.hqplayer_port == other.hqplayer_port
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputState {
    pub revision: u64,
    pub playback_generation: Option<u64>,
    pub configured: AudioOutputConfig,
    pub active: Option<AudioOutputConfig>,
    pub pending: bool,
}

impl AudioOutputState {
    pub fn new(configured: AudioOutputConfig, active: Option<AudioOutputConfig>) -> Self {
        let pending = active
            .as_ref()
            .is_some_and(|active| !configured.same_processing_as(active));
        Self {
            revision: 0,
            playback_generation: None,
            configured,
            active,
            pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_native_preferences_and_inactive_dsp_survive() {
        let mut settings = crate::Settings {
            bit_perfect: true,
            ..Default::default()
        };
        let native = AudioOutputConfig::from_settings(&settings);
        assert_eq!(native.route, AudioOutputRoute::Native);
        assert!(native.exclusive_mode);
        settings.camilla_fir = true;
        settings.camilla_config = Some("room.yml".into());
        let camilla = AudioOutputConfig::from_settings(&settings);
        assert_eq!(camilla.route, AudioOutputRoute::Camilla);
        assert!(camilla.bit_perfect);
        assert!(!camilla.effective().bit_perfect);
        assert!(camilla.effective().exclusive_mode);
        settings.bit_perfect = false;
        settings.exclusive_mode = false;
        assert_eq!(
            AudioOutputConfig::from_settings(&settings).route,
            AudioOutputRoute::Native
        );
        settings.hqplayer = true;
        assert_eq!(
            AudioOutputConfig::from_settings(&settings).route,
            AudioOutputRoute::Hqplayer
        );
    }

    #[test]
    fn missing_config_never_enables_legacy_dsp() {
        let settings = crate::Settings {
            camilla_fir: true,
            exclusive_mode: true,
            ..Default::default()
        };
        assert_eq!(
            AudioOutputConfig::from_settings(&settings).route,
            AudioOutputRoute::Native
        );
    }

    #[test]
    fn explicit_route_wins_and_updates_only_audio_fields() {
        let mut settings = crate::Settings {
            output_route: Some(AudioOutputRoute::Native),
            hqplayer: true,
            client_id: "keep-account".into(),
            volume: 0.4,
            ..Default::default()
        };
        let mut config = AudioOutputConfig::from_settings(&settings);
        assert_eq!(config.route, AudioOutputRoute::Native);
        config.route = AudioOutputRoute::Camilla;
        config.camilla_config = Some("room.yml".into());
        config.write_settings(&mut settings);
        assert_eq!(settings.client_id, "keep-account");
        assert_eq!(settings.volume, 0.4);
        assert!(!settings.hqplayer);
        assert!(settings.camilla_fir);
    }

    #[test]
    fn pending_compares_effective_processing_not_inactive_preferences() {
        let mut config = AudioOutputConfig {
            route: AudioOutputRoute::Hqplayer,
            ..Default::default()
        };
        let active = config.effective();
        config.bit_perfect = true;
        config.device = Some("hw:1".into());
        assert!(!AudioOutputState::new(config.clone(), Some(active.clone())).pending);
        config.hqplayer_port = 4322;
        assert!(AudioOutputState::new(config.clone(), Some(active)).pending);
        assert!(!AudioOutputState::new(config, None).pending);
    }

    #[test]
    fn unsupported_legacy_endpoint_is_preserved_until_explicitly_corrected() {
        let settings = crate::Settings {
            hqplayer: true,
            hqplayer_host: "192.168.1.2".into(),
            ..Default::default()
        };
        let mut config = AudioOutputConfig::from_settings(&settings);
        assert_eq!(config.hqplayer_host, "192.168.1.2");
        assert!(config.validate().is_err());
        config.route = AudioOutputRoute::Native;
        assert!(config.validate().is_ok());
        assert_eq!(config.hqplayer_host, "192.168.1.2");
    }

    #[test]
    fn local_endpoint_and_dsp_paths_are_validated() {
        let mut config = AudioOutputConfig {
            route: AudioOutputRoute::Camilla,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.camilla_config = Some("  room.yml  ".into());
        config.validate().unwrap();
        assert_eq!(config.camilla_config.as_deref(), Some("room.yml"));
        config.route = AudioOutputRoute::Hqplayer;
        config.hqplayer_host = " localhost ".into();
        config.validate().unwrap();
        assert_eq!(config.hqplayer_host, "127.0.0.1");
        config.hqplayer_port = 0;
        assert!(config.validate().is_err());
    }
}
