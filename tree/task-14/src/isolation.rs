//! Proof that the boxes in `runtime.rs` are actually isolated.
//!
//! Same standard as `verify.rs`: a claim counts only when it has a causal
//! signature, not when two outputs merely differ. Here the signature is a
//! secret token. Box *i* is told token *i* and nothing else; if box *i* can
//! recall token *i* while never producing token *j*, the sessions are really
//! separate contexts and not one shared history. `Leaked` and `Amnesiac` are
//! reported honestly rather than smoothed into a pass.
//!
//! Two halves:
//!   * structural — 100 boxes in one process, offline, no network at all;
//!   * live — 4 boxes on `glm-5.3-flash`, plus a close/resume round trip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::api::{Endpoint, DEFAULT_BASE_URL};
use crate::config::{Res, Settings};
use crate::runtime::{BoxSpec, Runtime};

/// Boxes stood up for the structural half. "Conditionally 100" — the point is
/// that nothing in the design is per-process singleton.
const STRUCTURAL_BOXES: usize = 100;

const TOKENS: [&str; 3] = ["MARZIPAN", "OBSIDIAN", "PELICAN"];

fn tell(token: &str) -> String {
    format!(
        "Remember this token for the rest of our conversation: {token}. Reply with only the word OK."
    )
}

const RECALL: &str = "What token did I ask you to remember earlier in this conversation? \
Reply with only that token, or with NONE if I never gave you one.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IsolationVerdict {
    /// Recalled its own token and produced no sibling's.
    Confirmed,
    /// Produced a token belonging to another box — sessions share memory.
    Leaked,
    /// Did not recall its own token; isolation untested, not proven.
    Amnesiac,
}

impl IsolationVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            IsolationVerdict::Confirmed => "Confirmed",
            IsolationVerdict::Leaked => "Leaked",
            IsolationVerdict::Amnesiac => "Amnesiac",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StructuralCheck {
    pub boxes: usize,
    pub distinct_ids: usize,
    pub distinct_files: usize,
    pub cross_talk: usize,
    pub resumed_recalled: bool,
    pub fresh_box_blank: bool,
}

impl StructuralCheck {
    pub fn verdict(&self) -> IsolationVerdict {
        if self.cross_talk > 0 {
            return IsolationVerdict::Leaked;
        }
        if self.distinct_ids == self.boxes
            && self.distinct_files == self.boxes
            && self.resumed_recalled
            && self.fresh_box_blank
        {
            IsolationVerdict::Confirmed
        } else {
            IsolationVerdict::Amnesiac
        }
    }
}

#[derive(Clone, Debug)]
pub struct BoxProbe {
    pub label: String,
    pub id: String,
    /// Token this box was told, if any.
    pub own_token: Option<&'static str>,
    /// Raw recall answer.
    pub recalled: String,
    /// Sibling tokens found in the answer.
    pub foreign: Vec<&'static str>,
    pub prompt_tokens: u64,
    pub turns_in_session: usize,
}

impl BoxProbe {
    pub fn verdict(&self) -> IsolationVerdict {
        if !self.foreign.is_empty() {
            return IsolationVerdict::Leaked;
        }
        match self.own_token {
            Some(t) if self.recalled.to_uppercase().contains(t) => IsolationVerdict::Confirmed,
            // The control box was told nothing; knowing nothing is the pass.
            None => IsolationVerdict::Confirmed,
            Some(_) => IsolationVerdict::Amnesiac,
        }
    }

    fn line(&self) -> String {
        format!(
            "{:<10} id={} own={} prompt_tokens={} turns={} recalled=`{}` foreign={:?} => {}",
            self.label,
            self.id,
            self.own_token.unwrap_or("-"),
            self.prompt_tokens,
            self.turns_in_session,
            clip(&self.recalled, 40),
            self.foreign,
            self.verdict().as_str()
        )
    }
}

#[derive(Clone, Debug)]
pub struct ResumeProbe {
    pub id: String,
    pub token: &'static str,
    pub recalled_after_resume: String,
    /// Turns the session file carried back into the fresh box.
    pub turns_restored: usize,
}

impl ResumeProbe {
    pub fn verdict(&self) -> IsolationVerdict {
        if self
            .recalled_after_resume
            .to_uppercase()
            .contains(self.token)
        {
            IsolationVerdict::Confirmed
        } else {
            IsolationVerdict::Amnesiac
        }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub endpoint: String,
    pub structural: StructuralCheck,
    pub live: Option<Vec<BoxProbe>>,
    pub resume: Option<ResumeProbe>,
}

impl Report {
    /// True only when every check that ran came back `Confirmed`.
    pub fn all_confirmed(&self) -> bool {
        self.structural.verdict() == IsolationVerdict::Confirmed
            && self
                .live
                .as_ref()
                .is_none_or(|p| p.iter().all(|b| b.verdict() == IsolationVerdict::Confirmed))
            && self
                .resume
                .as_ref()
                .is_none_or(|r| r.verdict() == IsolationVerdict::Confirmed)
    }

    pub fn render(&self) -> String {
        let s = &self.structural;
        let mut out = String::new();
        out.push_str("agent-box isolation self-test\n");
        out.push_str(&format!("endpoint: {}\n\n", self.endpoint));
        out.push_str(&format!(
            "== structural: {} boxes in one process (offline) ==\n\
             distinct ids={}/{}  distinct session files={}/{}\n\
             cross-talk between boxes: {}\n\
             resumed box recalled its own turns: {}\n\
             freshly spawned box saw nothing: {}\n=> {}\n\n",
            s.boxes,
            s.distinct_ids,
            s.boxes,
            s.distinct_files,
            s.boxes,
            s.cross_talk,
            s.resumed_recalled,
            s.fresh_box_blank,
            s.verdict().as_str()
        ));

        match &self.live {
            None => out.push_str("== live: skipped (--offline) ==\n\n"),
            Some(probes) => {
                out.push_str(&format!(
                    "== live: {} boxes, one runtime, secret-token recall ==\n",
                    probes.len()
                ));
                for p in probes {
                    out.push_str(&format!("{}\n", p.line()));
                }
                out.push('\n');
            }
        }

        if let Some(r) = &self.resume {
            out.push_str(&format!(
                "== live: close, drop, resume from disk ==\n\
                 id={} token={} turns restored={} recalled=`{}`\n=> {}\n\n",
                r.id,
                r.token,
                r.turns_restored,
                clip(&r.recalled_after_resume, 40),
                r.verdict().as_str()
            ));
        }

        out.push_str(if self.all_confirmed() {
            "RESULT: sessions are isolated — each box saw only its own context.\n"
        } else {
            "RESULT: NOT fully confirmed — read the per-check verdicts above.\n"
        });
        out
    }
}

fn clip(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

fn probe_settings() -> Settings {
    Settings {
        // The proof is about session memory, so keep AGENTS.md out of it.
        context_enabled: false,
        max_chars: Some(200),
        system_prompt: "You are a test fixture. Follow instructions literally and answer with as few words as possible."
            .into(),
        ..Settings::default()
    }
}

/// A fresh directory per call. Two runs inside one process (the unit tests
/// run in parallel) must not share a sessions dir, or each one counts the
/// other's files and the proof reads as a leak that is not there.
fn scratch_dir(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ask6-isolation-{label}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 100 boxes, no network: unique ids, unique files, no cross-talk, and a
/// close/resume round trip that only restores its own memory.
pub fn structural() -> Res<StructuralCheck> {
    let dir = scratch_dir("structural");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let mut rt = Runtime::with_endpoint(offline_endpoint(), dir.clone());
    let ids: Vec<String> = (0..STRUCTURAL_BOXES)
        .map(|i| rt.spawn(BoxSpec::new(format!("box-{i}"), probe_settings())))
        .collect();
    // `#` terminates the token so `STRUCT1#` is not found inside `STRUCT10#`.
    for (i, id) in ids.iter().enumerate() {
        rt.get_mut(id)
            .ok_or("spawned box vanished")?
            .seed(&format!("token STRUCT{i}#"), &format!("noted STRUCT{i}#"));
    }

    let mut cross_talk = 0usize;
    for (i, id) in ids.iter().enumerate() {
        let seen: String = rt
            .get(id)
            .ok_or("spawned box vanished")?
            .history()
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join(" ");
        for other in 0..STRUCTURAL_BOXES {
            if other != i && seen.contains(&format!("STRUCT{other}#")) {
                cross_talk += 1;
            }
        }
    }

    rt.save_all()?;
    let distinct_files = rt.list_saved().len();
    let distinct_ids = ids.iter().collect::<BTreeSet<_>>().len();

    let first = ids[0].clone();
    rt.close(&first)?;
    rt.resume(&first, BoxSpec::new("", probe_settings()))?;
    let resumed = rt.get(&first).ok_or("resumed box vanished")?.history();
    let resumed_recalled = resumed
        .first()
        .is_some_and(|m| m.content.contains("STRUCT0#"))
        && !resumed.iter().any(|m| m.content.contains("STRUCT1#"));

    let fresh = rt.spawn(BoxSpec::new("fresh", probe_settings()));
    let fresh_box_blank = rt
        .get(&fresh)
        .ok_or("fresh box vanished")?
        .history()
        .is_empty();

    let _ = std::fs::remove_dir_all(&dir);
    Ok(StructuralCheck {
        boxes: STRUCTURAL_BOXES,
        distinct_ids,
        distinct_files,
        cross_talk,
        resumed_recalled,
        fresh_box_blank,
    })
}

/// An endpoint that would fail on use. The structural half never sends.
fn offline_endpoint() -> Endpoint {
    Endpoint::unusable()
}

/// Live half: 3 boxes each told a different token plus one control box told
/// nothing, all inside one runtime; then a close/drop/resume recall.
fn live(dir: &Path) -> Res<(Vec<BoxProbe>, ResumeProbe)> {
    let mut rt = Runtime::with_endpoint(Endpoint::resolve()?, dir.to_path_buf());

    let mut ids = Vec::new();
    for (i, token) in TOKENS.iter().enumerate() {
        let id = rt.spawn(BoxSpec::new(format!("box-{i}"), probe_settings()));
        let turn = rt
            .get_mut(&id)
            .ok_or("spawned box vanished")?
            .ask(&tell(token))?;
        if !turn.accepted() {
            return Err(format!(
                "box-{i} could not be primed: {}",
                turn.refusal().unwrap_or_default()
            ));
        }
        ids.push((id, *token));
    }
    let control = rt.spawn(BoxSpec::new("control", probe_settings()));

    let mut probes = Vec::new();
    for (label, id, own) in ids
        .iter()
        .enumerate()
        .map(|(i, (id, t))| (format!("box-{i}"), id.clone(), Some(*t)))
        .chain(std::iter::once((
            "control".to_string(),
            control.clone(),
            None,
        )))
    {
        let turn = rt.get_mut(&id).ok_or("box vanished")?.ask(RECALL)?;
        let text = turn.text.clone();
        let upper = text.to_uppercase();
        let foreign: Vec<&'static str> = TOKENS
            .iter()
            .copied()
            .filter(|t| Some(*t) != own && upper.contains(t))
            .collect();
        probes.push(BoxProbe {
            label,
            own_token: own,
            recalled: text,
            foreign,
            prompt_tokens: turn
                .reply
                .as_ref()
                .map(|r| r.usage.prompt_tokens)
                .unwrap_or(0),
            turns_in_session: rt.get(&id).ok_or("box vanished")?.turns(),
            id,
        });
    }

    // Memory follows the session file, not the process: close the first box,
    // drop it, load it back, and ask again.
    let (first_id, first_token) = ids[0].clone();
    rt.close(&first_id)?;
    rt.resume(&first_id, BoxSpec::new("", probe_settings()))?;
    let turns_restored = rt.get(&first_id).ok_or("resumed box vanished")?.turns();
    let turn = rt
        .get_mut(&first_id)
        .ok_or("resumed box vanished")?
        .ask(RECALL)?;
    let resume = ResumeProbe {
        id: first_id,
        token: first_token,
        recalled_after_resume: turn.text,
        turns_restored,
    };

    rt.save_all()?;
    Ok((probes, resume))
}

/// Run the proof. `offline` skips every network call and reports only the
/// structural half.
pub fn run(offline: bool) -> Res<Report> {
    let structural = structural()?;
    if offline {
        return Ok(Report {
            endpoint: "(offline)".into(),
            structural,
            live: None,
            resume: None,
        });
    }
    let dir = scratch_dir("live");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let result = live(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    let (probes, resume) = result?;
    Ok(Report {
        endpoint: format!("{DEFAULT_BASE_URL}/chat/completions"),
        structural,
        live: Some(probes),
        resume: Some(resume),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_half_confirms_without_a_network() {
        let check = structural().unwrap();
        assert_eq!(check.boxes, STRUCTURAL_BOXES);
        assert_eq!(check.distinct_ids, STRUCTURAL_BOXES);
        assert_eq!(check.distinct_files, STRUCTURAL_BOXES);
        assert_eq!(check.cross_talk, 0);
        assert!(check.resumed_recalled);
        assert!(check.fresh_box_blank);
        assert_eq!(check.verdict(), IsolationVerdict::Confirmed);
    }

    #[test]
    fn offline_run_renders_and_skips_live() {
        let report = run(true).unwrap();
        assert!(report.live.is_none());
        assert!(report.resume.is_none());
        assert!(report.all_confirmed());
        let text = report.render();
        assert!(text.contains("100 boxes in one process"));
        assert!(text.contains("live: skipped"));
    }

    #[test]
    fn a_leak_is_reported_as_a_leak_not_smoothed_over() {
        let probe = BoxProbe {
            label: "box-0".into(),
            id: "x".into(),
            own_token: Some("MARZIPAN"),
            recalled: "MARZIPAN and also OBSIDIAN".into(),
            foreign: vec!["OBSIDIAN"],
            prompt_tokens: 10,
            turns_in_session: 2,
        };
        assert_eq!(probe.verdict(), IsolationVerdict::Leaked);

        let forgot = BoxProbe {
            foreign: vec![],
            recalled: "NONE".into(),
            ..probe.clone()
        };
        assert_eq!(forgot.verdict(), IsolationVerdict::Amnesiac);

        let control = BoxProbe {
            own_token: None,
            foreign: vec![],
            recalled: "NONE".into(),
            ..probe
        };
        assert_eq!(control.verdict(), IsolationVerdict::Confirmed);
    }

    #[test]
    fn structural_leak_outranks_the_other_signals() {
        let check = StructuralCheck {
            boxes: 100,
            distinct_ids: 100,
            distinct_files: 100,
            cross_talk: 1,
            resumed_recalled: true,
            fresh_box_blank: true,
        };
        assert_eq!(check.verdict(), IsolationVerdict::Leaked);
    }

    #[test]
    fn resume_probe_needs_the_token_back() {
        let ok = ResumeProbe {
            id: "x".into(),
            token: "PELICAN",
            recalled_after_resume: "pelican".into(),
            turns_restored: 2,
        };
        assert_eq!(ok.verdict(), IsolationVerdict::Confirmed);
        let bad = ResumeProbe {
            recalled_after_resume: "NONE".into(),
            ..ok
        };
        assert_eq!(bad.verdict(), IsolationVerdict::Amnesiac);
    }
}
