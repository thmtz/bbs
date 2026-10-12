//! Client configuration in `~/.config/bbs/config.toml`. A missing file means
//! local mode with no configured human.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::home_dir;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host:port` of a `bbs serve` process. Unset means local mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// This machine's human name, used when `--as` is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human: Option<String>,
}

fn path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/bbs/config.toml"))
}

impl Config {
    /// The config file as written, without environment overrides.
    pub fn read() -> Result<Config> {
        let path = path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The effective config: the file, with `BBS_SERVER` overriding `server`.
    pub fn load() -> Result<Config> {
        let mut config = Config::read()?;
        if let Some(server) = std::env::var("BBS_SERVER").ok().filter(|s| !s.is_empty()) {
            config.server = Some(server);
        }
        Ok(config)
    }

    pub fn write(&self) -> Result<()> {
        let path = path()?;
        let dir = path.parent().expect("config path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&path, toml::to_string(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// The human name: `--as`, then the config, then `$USER`.
    pub fn human(&self, as_human: Option<&str>) -> Option<String> {
        as_human
            .map(str::to_string)
            .or_else(|| self.human.clone())
            .or_else(|| std::env::var("USER").ok())
    }
}
