//! doctor.rs — environment preflight (`curam doctor`).
//!
//! H1: config sanity, workdir, cage binary, lists dir, terminal. Provider
//! reachability lands with H2; the fail-closed stance (missing cage = a
//! failed check, never a silent raw fallback) is already enforced here.

use std::io::IsTerminal;
use std::path::PathBuf;

use super::config;
use super::config::expand_home;

#[derive(Debug)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub fn run(cfg: &config::Config) -> Vec<Check> {
    let mut out = Vec::new();

    // config: implicit — load() already errored before us; the check shows
    // which file won and how many warnings were fielded.
    let source = cfg
        .config_path
        .as_ref()
        .map(|p| p.to_str().unwrap_or_default().to_string())
        .unwrap_or_else(|| "- (defaults)".into());
    let warn_note = if cfg.warnings.is_empty() {
        String::new()
    } else {
        format!(", {} warning(s) above", cfg.warnings.len())
    };
    out.push(Check {
        name: "config".into(),
        ok: true,
        detail: format!("loaded from {} {}", source, warn_note),
    });

    // workdir: must exist (create) and be a directory.
    let wd = &cfg.workdir;
    match std::fs::create_dir_all(wd) {
        Ok(_) if wd.is_dir() => {
            out.push(Check {
                name: "workdir".into(),
                ok: true,
                detail: format!("{} (writable)", wd.to_str().unwrap_or_default()),
            });
        }
        Ok(_) => {
            out.push(Check {
                name: "workdir".into(),
                ok: false,
                detail: format!("{} is not a directory", wd.to_str().unwrap_or_default()),
            });
        }
        Err(e) => {
            out.push(Check {
                name: "workdir".into(),
                ok: false,
                detail: format!("{}: {}", wd.to_str().unwrap_or_default(), e),
            });
        }
    }

    // cage: the single exec boundary must resolve to a real binary.
    match resolve_cage_bin(cfg) {
        Some(p) => {
            out.push(Check {
                name: "cage".into(),
                ok: true,
                detail: format!("curam-run @ {}", p.to_str().unwrap_or_default()),
            });
        }
        None => {
            out.push(Check {
                name: "cage".into(),
                ok: false,
                detail: "curam-run not found — shell/run tools fail closed until it exists. "
                    .into(),
            });
        }
    }

    // lists: absent is tolerated (built-in defaults); a present dir is noted.
    let lists = expand_home(cfg.cage.lists_dir.as_str());
    if lists.is_dir() {
        out.push(Check {
            name: "lists".into(),
            ok: true,
            detail: lists.to_str().unwrap_or_default().to_string(),
        });
    } else {
        out.push(Check {
            name: "lists".into(),
            ok: true,
            detail: format!(
                "no lists at {} — match_host uses built-in defaults",
                lists.to_str().unwrap_or_default()
            ),
        });
    }

    // terminal: informational for H1 (TUI/REPL land in H5).
    let tty = std::io::stdin().is_terminal();
    let term = if tty {
        "interactive tty"
    } else {
        "non-interactive (piped)"
    };
    out.push(Check {
        name: "terminal".into(),
        ok: true,
        detail: term.into(),
    });

    out
}

/// Resolve the cage binary: CURAM_RUN_BIN env > [cage] binary (path or on
/// PATH) > the POC's default dev build path.
fn resolve_cage_bin(cfg: &config::Config) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CURAM_RUN_BIN") {
        return Some(PathBuf::from(p));
    }
    let from_cfg = expand_home(cfg.cage.binary.as_str());
    let bin: String = format!("{from_cfg:?}");
    if bin.contains('/') && from_cfg.exists() {
        return Some(from_cfg);
    }
    if let Some(dir) = path_search(bin.as_str()) {
        return Some(dir);
    }
    let dev = std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join("code")
                .join("curam-security")
                .join("target")
                .join("debug")
                .join("curam-run")
        })
        .unwrap_or(PathBuf::from("/nonexistent/curam-run"));
    if dev.exists() {
        Some(dev)
    } else {
        None
    }
}

fn path_search(bin: &str) -> Option<PathBuf> {
    let path: String = std::env::var_os("PATH")
        .map(|p| {
            std::path::PathBuf::from(p)
                .to_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    path.split(':')
        .map(|d| PathBuf::from(d).join(bin))
        .find(|cand| cand.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_binary(binary: &str) -> config::Config {
        let mut cfg = config::Loader::new(None, None).load().unwrap();
        cfg.cage.binary = binary.into();
        cfg
    }

    #[test]
    fn resolve_absolute_binary() {
        let cfg = cfg_with_binary("/bin/sh");
        assert_eq!(resolve_cage_bin(&cfg), Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn path_search_finds_bin_on_path() {
        let found = path_search("sh");
        assert!(found.is_some(), "sh must resolve on PATH");
    }

    #[test]
    fn doctor_run_shapes_checks() {
        let cfg = config::Loader::new(None, None).load().unwrap();
        let checks = run(&cfg);
        assert!(checks.iter().all(|c| !c.name.is_empty()));
        let wd = checks.iter().find(|c| c.name == "workdir").unwrap();
        assert_eq!(wd.ok, true);
    }
}
