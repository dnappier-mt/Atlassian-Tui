use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use std::path::PathBuf;

const APP: &str = "jui";

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "jui", APP).context("could not resolve project directories")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

pub fn cache_db() -> Result<PathBuf> {
    Ok(data_dir()?.join("cache.sqlite"))
}

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(xdg).join(APP);
        return Ok(p);
    }
    let base = BaseDirs::new().context("no base dirs")?;
    Ok(base.cache_dir().join(APP))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("jui.sock"))
}

pub fn pid_file() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("jui.pid"))
}

pub fn status_file() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("status"))
}

pub fn ensure_dirs() -> Result<()> {
    std::fs::create_dir_all(config_dir()?)?;
    std::fs::create_dir_all(data_dir()?)?;
    std::fs::create_dir_all(runtime_dir()?)?;
    Ok(())
}
