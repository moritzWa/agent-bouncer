use crate::headroom::History;
use crate::state::*;
use crate::stats;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CACHE_TTL_MS: u64 = 60 * 60 * 1000;
const DOWN_FOR_MS: u64 = 60 * 1000;
const SIMILAR_RUNS: usize = 15;

#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    pub ram_mb: u64,
    pub cores: f64,
    /// Where the numbers came from: model, cache, history or default.
    pub source: &'static str,
}

/// What a job is expected to need: the model's estimate from similar past runs,
/// or the past runs alone when there is no model, no key, or no answer in time.
pub fn predict(
    root: &Path,
    settings: &Settings,
    label: Option<&str>,
    cmd: &str,
    cwd: &str,
    running: &[SlotInfo],
) -> Prediction {
    let events = read_recent_events(root);
    let history = History::from_events(&events);
    let fallback = Prediction {
        ram_mb: history.predict_mb(label, cmd),
        cores: history.predict_cores(label, cmd),
        source: if history.knows(label, cmd) { "history" } else { "default" },
    };
    let Some(model) = settings.model.as_deref() else { return fallback };

    let key = stats::key_of(label, cmd);
    let cache = cache_path(root, &key, cwd);
    if let Some(hit) = read_cache(&cache) {
        return hit;
    }
    if model_is_down(root) {
        return fallback;
    }
    let Some(api_key) = api_key() else { return fallback };

    let prompt = build_prompt(&events, label, cmd, cwd, running, crate::sysinfo::cores());
    let Some((ram, cores)) = ask(model, &api_key, &prompt, settings.model_timeout_secs) else {
        mark_down(root);
        return fallback;
    };
    let mut ram_mb = (ram.round() as u64).clamp(50, 65_536);
    let mut cores = cores.clamp(0.1, crate::sysinfo::cores() as f64);
    if let Some((usual_mb, usual_cores)) = history.typical(label, cmd) {
        ram_mb = ram_mb.max(usual_mb);
        cores = cores.max(usual_cores.unwrap_or(0.0));
    }
    let prediction = Prediction { ram_mb, cores: (cores * 10.0).round() / 10.0, source: "model" };
    write_cache(&cache, &prediction);
    prediction
}

fn api_key() -> Option<String> {
    let from_env = |var: &str| std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    from_env("AGENT_BOUNCER_OPENROUTER_KEY")
        .or_else(|| {
            let home = std::env::var("HOME").ok()?;
            let text = std::fs::read_to_string(PathBuf::from(home).join(".config/agent-bouncer/openrouter-key")).ok()?;
            Some(text.trim().to_string()).filter(|k| !k.is_empty())
        })
        .or_else(|| from_env("OPENROUTER_API_KEY"))
}

const SYSTEM_PROMPT: &str = "You predict how much of a computer one shell job will use at its busiest, \
so a queue can start jobs that fit together without freezing the machine. \
ram_mb is peak resident memory in MB summed over every process the job starts. \
cores is peak CPU in cores summed over every process the job starts (test runners and bundlers often start many workers). \
Past runs of similar commands are your best evidence: weigh the closest ones most, and notice when the same command used very different amounts in different directories. \
For a command with no similar runs, judge from the tool and what it does. \
Answer with JSON only: {\"ram_mb\": integer, \"cores\": number}.";

fn build_prompt(
    events: &[Event],
    label: Option<&str>,
    cmd: &str,
    cwd: &str,
    running: &[SlotInfo],
    machine_cores: usize,
) -> String {
    let key = stats::key_of(label, cmd);
    let first = key.split_whitespace().next().unwrap_or("").to_string();
    let mut similar: Vec<(u32, &Event)> = events
        .iter()
        .filter(|e| e.ev == "finish" && e.peak_rss_mb.is_some_and(|mb| mb > 0))
        .filter_map(|e| {
            let their_key = stats::command_key(e);
            let mut score = 0;
            if their_key == key {
                score += 4;
            } else if their_key.split_whitespace().next() == Some(first.as_str()) && !first.is_empty() {
                score += 2;
            }
            if e.cwd.as_deref() == Some(cwd) {
                score += 1;
            }
            (score >= 2).then_some((score, e))
        })
        .collect();
    similar.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.ts.cmp(&a.1.ts)));
    let runs: Vec<Value> = similar
        .iter()
        .take(SIMILAR_RUNS)
        .map(|(_, e)| {
            json!({
                "command": clip(e.cmd.as_deref().unwrap_or(""), 160),
                "cwd": e.cwd,
                "peak_ram_mb": e.peak_rss_mb,
                "peak_cores": e.peak_cores,
                "seconds": e.dur_ms.map(|ms| ms / 1000),
            })
        })
        .collect();
    let others: Vec<Value> = running
        .iter()
        .map(|job| json!({"command": clip(&job.cmd, 100), "expected_ram_mb": job.est_peak_mb, "expected_cores": job.est_cores}))
        .collect();
    let mem = crate::sysinfo::memory();
    json!({
        "command": clip(cmd, 400),
        "label": label,
        "cwd": cwd,
        "machine": {
            "cores": machine_cores,
            "free_ram_gb": mem.map(|m| (m.avail_gb * 10.0).round() / 10.0),
        },
        "running_now": others,
        "similar_past_runs": runs,
    })
    .to_string()
}

fn clip(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ; ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(max).collect();
    cut.push('…');
    cut
}

/// One call to OpenRouter through curl, with the key passed on stdin so it never
/// shows in the process list.
fn ask(model: &str, api_key: &str, prompt: &str, timeout_secs: u64) -> Option<(f64, f64)> {
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ],
        "response_format": {"type": "json_object"},
        "max_tokens": 120,
        "temperature": 0,
    })
    .to_string();
    let mut child = Command::new("curl")
        .args([
            "-s",
            "-f",
            "-m",
            &timeout_secs.to_string(),
            "-K",
            "-",
            "https://openrouter.ai/api/v1/chat/completions",
            "-H",
            "content-type: application/json",
            "-H",
            "X-Title: agent-bouncer",
            "-d",
            &body,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(format!("header = \"Authorization: Bearer {api_key}\"\n").as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_answer(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_answer(response: &str) -> Option<(f64, f64)> {
    let value: Value = serde_json::from_str(response).ok()?;
    let content = value.pointer("/choices/0/message/content")?.as_str()?;
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    let answer: Value = serde_json::from_str(&content[start..=end]).ok()?;
    let ram = answer.get("ram_mb")?.as_f64()?;
    let cores = answer.get("cores")?.as_f64()?;
    (ram.is_finite() && cores.is_finite() && ram > 0.0 && cores > 0.0).then_some((ram, cores))
}

fn cache_path(root: &Path, key: &str, cwd: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    (key, cwd).hash(&mut hasher);
    root.join("predict-cache").join(format!("{:016x}.json", hasher.finish()))
}

fn read_cache(path: &Path) -> Option<Prediction> {
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let at = value.get("ts")?.as_u64()?;
    if now_ms().saturating_sub(at) > CACHE_TTL_MS {
        return None;
    }
    Some(Prediction {
        ram_mb: value.get("ram_mb")?.as_u64()?,
        cores: value.get("cores")?.as_f64()?,
        source: "cache",
    })
}

fn write_cache(path: &Path, prediction: &Prediction) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = json!({"ts": now_ms(), "ram_mb": prediction.ram_mb, "cores": prediction.cores}).to_string();
    write_atomic(path, json.as_bytes());
}

/// After a failed call, skip the model for a minute so a bad network or key does
/// not slow every job.
fn down_path(root: &Path) -> PathBuf {
    root.join("model-down")
}

fn mark_down(root: &Path) {
    write_atomic(&down_path(root), now_ms().to_string().as_bytes());
}

fn model_is_down(root: &Path) -> bool {
    std::fs::read_to_string(down_path(root))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .is_some_and(|at| now_ms().saturating_sub(at) < DOWN_FOR_MS)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_models_json_even_when_wrapped_in_text() {
        let reply = r#"{"choices":[{"message":{"content":"```json\n{\"ram_mb\": 4800, \"cores\": 7.5}\n```"}}]}"#;
        assert_eq!(parse_answer(reply), Some((4800.0, 7.5)));
        assert_eq!(parse_answer(r#"{"error":{"code":401}}"#), None);
        let zero = r#"{"choices":[{"message":{"content":"{\"ram_mb\": 0, \"cores\": 1}"}}]}"#;
        assert_eq!(parse_answer(zero), None);
    }

    #[test]
    fn the_prompt_carries_the_closest_past_runs_first() {
        let finish = |cmd: &str, cwd: &str, mb: u64, ts: u64| Event {
            ts,
            ev: "finish".into(),
            cmd: Some(cmd.into()),
            cwd: Some(cwd.into()),
            peak_rss_mb: Some(mb),
            peak_cores: Some(3.0),
            ..Default::default()
        };
        let events = vec![
            finish("cargo build", "/a", 111, 1),
            finish("bun run ship", "/a", 222, 2),
            finish("bun run ship", "/b", 333, 3),
            finish("bun run format", "/b", 444, 4),
        ];
        let prompt = build_prompt(&events, None, "bun run ship", "/a", &[], 18);
        let at = |needle: &str| prompt.find(needle);
        assert!(at("222").unwrap() < at("333").unwrap());
        assert!(at("333").unwrap() < at("444").unwrap());
        assert!(at("111").is_none());
    }

    #[test]
    fn long_multiline_commands_are_clipped_to_one_line() {
        let clipped = clip("set -e\ncd x\nbun run test", 10);
        assert!(!clipped.contains('\n'));
        assert!(clipped.ends_with('…'));
    }
}
