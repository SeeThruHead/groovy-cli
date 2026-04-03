use anyhow::{Context, Result};
use crate::groovy::Modeline;
use crate::groovy::MODELINES;
use crate::plex;

#[derive(serde::Deserialize, Default)]
pub struct Config {
    pub mister: Option<String>,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub modeline: Option<String>,
    pub scale: Option<f64>,
    pub custom_modeline: Option<CustomModeline>,
}

#[derive(serde::Deserialize, Clone)]
pub struct CustomModeline {
    pub p_clock: f64,
    pub h_active: u16,
    pub h_begin: u16,
    pub h_end: u16,
    pub h_total: u16,
    pub v_active: u16,
    pub v_begin: u16,
    pub v_end: u16,
    pub v_total: u16,
    pub interlace: bool,
}

pub struct ResolvedConfig {
    pub mister: String,
    pub server: String,
    pub port: u16,
    pub token: String,
    pub modeline_name: String,
    pub scale: f64,
    pub custom_modeline: Option<CustomModeline>,
}

impl ResolvedConfig {
    pub fn modeline(&self) -> Result<Modeline> {
        if let Some(ref cm) = self.custom_modeline {
            return Ok(Modeline {
                name: "custom",
                p_clock: cm.p_clock,
                h_active: cm.h_active, h_begin: cm.h_begin,
                h_end: cm.h_end, h_total: cm.h_total,
                v_active: cm.v_active, v_begin: cm.v_begin,
                v_end: cm.v_end, v_total: cm.v_total,
                interlace: cm.interlace,
            });
        }
        MODELINES.iter().find(|m| m.name == self.modeline_name).copied().with_context(|| {
            format!("Unknown modeline '{}'. Available:\n  {}",
                self.modeline_name,
                MODELINES.iter().map(|m| m.name).collect::<Vec<_>>().join("\n  "))
        })
    }

    pub fn plex(&self) -> plex::PlexClient {
        plex::PlexClient::new(&self.server, self.port, &self.token)
    }
}

pub fn config_path() -> std::path::PathBuf {
    let xdg = dirs::home_dir()
        .map(|h| h.join(".config").join("groovy-cli").join("config.toml"))
        .unwrap_or_default();
    if xdg.exists() { return xdg; }
    dirs::config_dir()
        .map(|d| d.join("groovy-cli").join("config.toml"))
        .unwrap_or(xdg)
}

pub fn load() -> Config {
    let path = config_path();
    if path.exists() {
        toml::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).unwrap_or_default()
    } else {
        Config::default()
    }
}

/// Resolve CLI args + config file + env vars into a single config
pub fn resolve(
    mister: Option<String>, server: Option<String>, port: Option<u16>,
    token: Option<String>, modeline: Option<String>, scale: Option<f64>,
    cfg: &Config,
) -> Result<ResolvedConfig> {
    Ok(ResolvedConfig {
        mister: mister.or_else(|| cfg.mister.clone()).unwrap_or_else(|| "192.168.0.115".into()),
        server: server.or_else(|| cfg.server.clone()).unwrap_or_else(|| "localhost".into()),
        port: port.or(cfg.port).unwrap_or(32400),
        token: token.or_else(|| cfg.token.clone()).context(
            "No Plex token. Set via --token, PLEX_TOKEN env, or 'token' in ~/.config/groovy-cli/config.toml")?,
        modeline_name: modeline.or_else(|| cfg.modeline.clone()).unwrap_or_else(|| "640x480i NTSC".into()),
        scale: scale.or(cfg.scale).unwrap_or(1.0).clamp(0.3, 1.0),
        custom_modeline: cfg.custom_modeline.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_defaults() {
        let cfg = Config::default();
        // Can't resolve without token
        assert!(resolve(None, None, None, None, None, None, &cfg).is_err());
    }

    #[test]
    fn test_resolve_with_token() {
        let cfg = Config { token: Some("test".into()), ..Default::default() };
        let r = resolve(None, None, None, None, None, None, &cfg).unwrap();
        assert_eq!(r.mister, "192.168.0.115");
        assert_eq!(r.port, 32400);
        assert_eq!(r.scale, 1.0);
        assert_eq!(r.modeline_name, "640x480i NTSC");
    }

    #[test]
    fn test_resolve_cli_overrides() {
        let cfg = Config { token: Some("test".into()), scale: Some(0.9), ..Default::default() };
        let r = resolve(
            Some("10.0.0.1".into()), None, Some(8080),
            None, Some("320x240 NTSC".into()), Some(0.85), &cfg
        ).unwrap();
        assert_eq!(r.mister, "10.0.0.1");
        assert_eq!(r.port, 8080);
        assert_eq!(r.scale, 0.85);
        assert_eq!(r.modeline_name, "320x240 NTSC");
    }

    #[test]
    fn test_scale_clamping() {
        let cfg = Config { token: Some("t".into()), ..Default::default() };
        let r = resolve(None, None, None, None, None, Some(0.1), &cfg).unwrap();
        assert_eq!(r.scale, 0.3);
        let r = resolve(None, None, None, None, None, Some(5.0), &cfg).unwrap();
        assert_eq!(r.scale, 1.0);
    }

    #[test]
    fn test_modeline_lookup() {
        let cfg = Config { token: Some("t".into()), ..Default::default() };
        let r = resolve(None, None, None, None, Some("640x480i NTSC".into()), None, &cfg).unwrap();
        let m = r.modeline().unwrap();
        assert_eq!(m.h_active, 640);
        assert_eq!(m.v_active, 480);
        assert!(m.interlace);
    }

    #[test]
    fn test_modeline_unknown() {
        let cfg = Config { token: Some("t".into()), ..Default::default() };
        let r = resolve(None, None, None, None, Some("bogus".into()), None, &cfg).unwrap();
        assert!(r.modeline().is_err());
    }

    #[test]
    fn test_custom_modeline() {
        let cfg = Config {
            token: Some("t".into()),
            custom_modeline: Some(CustomModeline {
                p_clock: 6.7, h_active: 320, h_begin: 336, h_end: 367, h_total: 426,
                v_active: 240, v_begin: 244, v_end: 247, v_total: 262, interlace: false,
            }),
            ..Default::default()
        };
        let r = resolve(None, None, None, None, None, None, &cfg).unwrap();
        let m = r.modeline().unwrap();
        assert_eq!(m.name, "custom");
        assert_eq!(m.h_active, 320);
        assert!(!m.interlace);
    }
}
