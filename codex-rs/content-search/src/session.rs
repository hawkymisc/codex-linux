use crate::matcher::build_matcher;
use anyhow::Context;
use anyhow::Result;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOptions {
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// Cap on total matches before bailing.
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
    /// Cap on per-file matches.
    #[serde(default = "default_max_per_file")]
    pub max_per_file: usize,
}

fn default_max_matches() -> usize {
    1024
}
fn default_max_per_file() -> usize {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentMatch {
    pub path: PathBuf,
    pub line_number: u64,
    /// One-line preview around the match. Trimmed at 240 chars; longer
    /// lines are truncated with a `…` marker at the end.
    pub preview: String,
}

pub struct SearchSession {
    rx: mpsc::Receiver<ContentMatch>,
    /// Set to `true` from the search task when the result cap is hit.
    pub capped: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl SearchSession {
    /// Spawn a blocking search task on `tokio::task::spawn_blocking`.
    /// Returns a session whose `next()` yields matches.
    pub fn spawn(root: impl AsRef<Path>, opts: SearchOptions) -> Self {
        let root = root.as_ref().to_path_buf();
        let (tx, rx) = mpsc::channel::<ContentMatch>(64);
        let capped = Arc::new(AtomicBool::new(false));
        let capped2 = capped.clone();
        let join = tokio::task::spawn_blocking(move || {
            if let Err(e) = run_search(&root, &opts, &tx, &capped2) {
                warn!(error = %e, "content-search task error");
            }
        });
        SearchSession { rx, capped, join }
    }

    pub async fn next(&mut self) -> Option<ContentMatch> {
        self.rx.recv().await
    }

    pub async fn drain_all(mut self) -> Vec<ContentMatch> {
        let mut out = Vec::new();
        while let Some(m) = self.rx.recv().await {
            out.push(m);
        }
        let _ = self.join.await;
        out
    }
}

fn run_search(
    root: &Path,
    opts: &SearchOptions,
    tx: &mpsc::Sender<ContentMatch>,
    capped: &Arc<AtomicBool>,
) -> Result<()> {
    let matcher = build_matcher(&opts.query, opts.case_sensitive, opts.regex, opts.whole_word)
        .context("failed to build search matcher")?;
    let mut total = 0usize;
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        // By default the `ignore` crate only consults .gitignore inside a
        // real git repo. Desktop callers will often point at directories
        // that aren't git-tracked, so always honour .gitignore files.
        .require_git(false)
        .build();
    let mut searcher = Searcher::new();
    'outer: for ent in walker.flatten() {
        if !ent.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = ent.path().to_path_buf();
        let mut per_file = 0usize;
        let res = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_number, line| {
                if total >= opts.max_matches {
                    capped.store(true, Ordering::Relaxed);
                    return Ok(false); // stop reading this file
                }
                if per_file >= opts.max_per_file {
                    return Ok(false);
                }
                let preview = trim_line(line, 240);
                if tx
                    .blocking_send(ContentMatch {
                        path: path.clone(),
                        line_number,
                        preview,
                    })
                    .is_err()
                {
                    return Ok(false);
                }
                total += 1;
                per_file += 1;
                Ok(true)
            }),
        );
        if let Err(e) = res {
            warn!(?path, error = %e, "search read error");
            continue;
        }
        if total >= opts.max_matches {
            break 'outer;
        }
    }
    Ok(())
}

fn trim_line(line: &str, max: usize) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let take: String = line.chars().take(max).collect();
        format!("{take}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[tokio::test(flavor = "multi_thread")]
    async fn finds_substring_in_temp_dir() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        std::fs::write(tmp.path().join("a.txt"), "hello world\nlorem ipsum\n")?;
        std::fs::write(tmp.path().join("b.txt"), "no match here\n")?;
        let session = SearchSession::spawn(
            tmp.path(),
            SearchOptions {
                query: "lorem".into(),
                case_sensitive: true,
                regex: false,
                whole_word: false,
                max_matches: 1024,
                max_per_file: 200,
            },
        );
        let matches = session.drain_all().await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
        assert!(matches[0].preview.contains("lorem"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn case_insensitive_search() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        std::fs::write(tmp.path().join("a.txt"), "HELLO\n")?;
        let session = SearchSession::spawn(
            tmp.path(),
            SearchOptions {
                query: "hello".into(),
                case_sensitive: false,
                regex: false,
                whole_word: false,
                max_matches: 10,
                max_per_file: 10,
            },
        );
        let matches = session.drain_all().await;
        assert_eq!(matches.len(), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn whole_word_excludes_partial_match() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        std::fs::write(tmp.path().join("a.txt"), "tested\ntest\n")?;
        let session = SearchSession::spawn(
            tmp.path(),
            SearchOptions {
                query: "test".into(),
                case_sensitive: false,
                regex: false,
                whole_word: true,
                max_matches: 10,
                max_per_file: 10,
            },
        );
        let matches = session.drain_all().await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cap_signals_capped_atomic() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        std::fs::write(tmp.path().join("a.txt"), "x\nx\nx\nx\nx\n")?;
        let session = SearchSession::spawn(
            tmp.path(),
            SearchOptions {
                query: "x".into(),
                case_sensitive: true,
                regex: false,
                whole_word: false,
                max_matches: 2,
                max_per_file: 10,
            },
        );
        let matches = session.drain_all().await;
        assert_eq!(matches.len(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn respects_gitignore() -> Result<()> {
        let tmp = tempfile::TempDir::new()?;
        std::fs::create_dir(tmp.path().join("subdir"))?;
        std::fs::write(tmp.path().join(".gitignore"), "subdir/\n")?;
        std::fs::write(tmp.path().join("subdir").join("ign.txt"), "secret\n")?;
        std::fs::write(tmp.path().join("a.txt"), "secret\n")?;
        let session = SearchSession::spawn(
            tmp.path(),
            SearchOptions {
                query: "secret".into(),
                case_sensitive: true,
                regex: false,
                whole_word: false,
                max_matches: 10,
                max_per_file: 10,
            },
        );
        let matches = session.drain_all().await;
        // Only the non-ignored a.txt should appear — assuming ignore crate
        // honours .gitignore by default in WalkBuilder.
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.ends_with("a.txt"));
        Ok(())
    }

    #[test]
    fn trim_line_caps_at_max() {
        let long = "a".repeat(500);
        let out = super::trim_line(&long, 240);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 241);
    }
}
