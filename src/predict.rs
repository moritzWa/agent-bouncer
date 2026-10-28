use crate::headroom::{percentile_f, History};
use crate::state::*;
use crate::stats;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const DOWN_FOR_MS: u64 = 60 * 1000;
const SIMILAR_RUNS: usize = 15;
const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    pub ram_mb: u64,
    pub cores: f64,
    /// Where the numbers came from: model, cache, history or default.
    pub source: &'static str,
    /// What the model answered before the sanity limits and history floor, and how long it took.
    pub raw: Option<(u64, f64, u64)>,
}

/// What the agent that asked to run the job says it will need. It knows what it is
/// about to do, so the model gets it as one more clue next to the measured runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hint {
    pub ram_mb: Option<u64>,
    pub cores: Option<f64>,
    pub secs: Option<u64>,
}

impl Hint {
    pub fn is_empty(&self) -> bool {
        self.ram_mb.is_none() && self.cores.is_none() && self.secs.is_none()
    }
}

/// `4gb`, `800mb`, `2.5g` or a bare number of MB.
pub fn parse_mb(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    let (num, unit) = match t.find(|c: char| c.is_alphabetic()) {
        Some(i) => (&t[..i], &t[i..]),
        None => (t.as_str(), "mb"),
    };
    let n: f64 = num.trim().parse().ok()?;
    let mb = match unit.trim() {
        "mb" | "m" => n,
        "gb" | "g" => n * 1024.0,
        "kb" | "k" => n / 1024.0,
        _ => return None,
    };
    (mb.is_finite() && mb > 0.0).then_some(mb.round() as u64)
}

/// `90s`, `3m`, `2h` or a bare number of seconds.
pub fn parse_secs(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    let (num, unit) = match t.find(|c: char| c.is_alphabetic()) {
        Some(i) => (&t[..i], &t[i..]),
        None => (t.as_str(), "s"),
    };
    let n: f64 = num.trim().parse().ok()?;
    let secs = match unit.trim() {
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    (secs.is_finite() && secs > 0.0).then_some(secs.round() as u64)
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
    hint: &Hint,
) -> Prediction {
    let events = read_recent_events(root);
    let history = History::from_events(&events);
    let fallback = Prediction {
        ram_mb: history.predict_mb(label, cmd),
        cores: history.predict_cores(label, cmd),
        source: if history.knows(label, cmd) {
            "history"
        } else {
            "default"
        },
        raw: None,
    };
    let Some(model) = settings.model.as_deref() else {
        return fallback;
    };

    let key = stats::key_of(label, cmd);
    let cache = cache_path(root, &key, cwd, hint);
    if let Some(hit) = read_cache(&cache, &events, &key) {
        return hit;
    }
    if model_is_down(root) {
        return fallback;
    }
    let Some((api_key, _)) = api_key() else {
        return fallback;
    };

    let prompt = build_prompt(
        &events,
        label,
        cmd,
        cwd,
        running,
        hint,
        gather_context(cwd, cmd),
    );
    let asked_at = std::time::Instant::now();
    let Some((ram, cores)) = ask(model, &api_key, &prompt, settings.model_timeout_secs) else {
        mark_down(root);
        return fallback;
    };
    let model_ms = asked_at.elapsed().as_millis() as u64;
    let mut ram_mb = (ram.round() as u64).clamp(50, 65_536);
    let cores_raw = cores;
    let mut cores = cores.clamp(0.1, crate::sysinfo::cores() as f64);
    if let Some((floor_mb, floor_cores)) = history_floor(&events, &key, cwd) {
        ram_mb = ram_mb.max(floor_mb);
        cores = cores.max(floor_cores.unwrap_or(0.0));
    }
    let prediction = Prediction {
        ram_mb,
        cores: (cores * 10.0).round() / 10.0,
        source: "model",
        raw: Some((
            ram.round().max(0.0) as u64,
            (cores_raw * 10.0).round() / 10.0,
            model_ms,
        )),
    };
    write_cache(&cache, &prediction);
    prediction
}

/// Exactly what the model would be sent for this job, for `predict --show`.
pub fn show_prompt(
    root: &Path,
    label: Option<&str>,
    cmd: &str,
    cwd: &str,
    running: &[SlotInfo],
    hint: &Hint,
) -> String {
    let events = read_recent_events(root);
    let prompt = build_prompt(
        &events,
        label,
        cmd,
        cwd,
        running,
        hint,
        gather_context(cwd, cmd),
    );
    let pretty = serde_json::from_str::<Value>(&prompt)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or(prompt);
    format!("--- system ---\n{SYSTEM_PROMPT}\n\n--- user ---\n{pretty}\n")
}

/// Where a floor under the model's answer comes from. Same command in the same
/// folder, three or more times: what it mostly stayed under (75th percentile), so
/// a low guess cannot go below what really happened there. Otherwise, with five or
/// more runs of the command anywhere: only the lowest quarter, which still catches
/// an absurd answer but lets the model be right that this run is small.
fn history_floor(events: &[Event], key: &str, cwd: &str) -> Option<(u64, Option<f64>)> {
    let same: Vec<&Event> = events
        .iter()
        .filter(|e| {
            e.ev == "finish"
                && e.peak_rss_mb.is_some_and(|mb| mb > 0)
                && stats::command_key(e) == key
        })
        .collect();
    let spread = |runs: &[&Event], p: f64| -> Option<(u64, Option<f64>)> {
        let mut ram: Vec<u64> = runs.iter().filter_map(|e| e.peak_rss_mb).collect();
        ram.sort_unstable();
        let mut cores: Vec<f64> = runs.iter().filter_map(|e| e.peak_cores).collect();
        cores.sort_by(|a, b| a.total_cmp(b));
        let cores = (cores.len() >= 3).then(|| percentile_f(&cores, p));
        Some((stats::percentile(&ram, p), cores))
    };
    let here: Vec<&Event> = same
        .iter()
        .copied()
        .filter(|e| e.cwd.as_deref() == Some(cwd))
        .collect();
    if here.len() >= 3 {
        spread(&here, 0.75)
    } else if same.len() >= 5 {
        spread(&same, 0.25)
    } else {
        None
    }
}

/// Where the key came from, so `doctor` can say.
fn api_key() -> Option<(String, &'static str)> {
    let from_env = |var: &str| {
        std::env::var(var)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let Some(k) = from_env("AGENT_BOUNCER_OPENROUTER_KEY") {
        return Some((k, "AGENT_BOUNCER_OPENROUTER_KEY"));
    }
    let file = std::env::var("HOME")
        .ok()
        .and_then(|home| {
            std::fs::read_to_string(
                PathBuf::from(home).join(".config/agent-bouncer/openrouter-key"),
            )
            .ok()
        })
        .map(|t| t.trim().to_string())
        .filter(|k| !k.is_empty());
    if let Some(k) = file {
        return Some((k, "~/.config/agent-bouncer/openrouter-key"));
    }
    from_env("OPENROUTER_API_KEY").map(|k| (k, "OPENROUTER_API_KEY"))
}

/// A real test call, so a wrong key shows up now and not as a silent fallback.
pub fn doctor(settings: &Settings) -> (bool, String) {
    let Some(model) = settings.model.as_deref() else {
        return (
            true,
            "model off (AGENT_BOUNCER_MODEL=off): predicting from past runs only".into(),
        );
    };
    let Some((key, source)) = api_key() else {
        return (
            false,
            "no OpenRouter key found. Set OPENROUTER_API_KEY, or put one in ~/.config/agent-bouncer/openrouter-key. Until then it predicts from past runs only.".into(),
        );
    };
    let started = std::time::Instant::now();
    let Some(body) = ask_raw(
        model,
        &key,
        "Answer with JSON only: {\"ram_mb\": 100, \"cores\": 1}.",
        "ping",
        10,
    ) else {
        return (
            false,
            format!("key found in {source}, but openrouter.ai did not answer (network down?)"),
        );
    };
    let secs = started.elapsed().as_secs_f64();
    if parse_answer(&body).is_some() {
        (
            true,
            format!("works: {model} answered in {secs:.1}s, key from {source}"),
        )
    } else {
        let why = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .and_then(|m| m.as_str().map(String::from))
            })
            .unwrap_or_else(|| body.chars().take(120).collect());
        (false, format!("key from {source} was refused: {why}"))
    }
}

const SYSTEM_PROMPT: &str = "You predict how much of a computer one shell job will use at its busiest, \
so a queue can start jobs that fit together without freezing the machine. \
ram_mb is peak resident memory in MB summed over every process the job starts. \
cores is peak CPU in cores summed over every process the job starts (test runners and bundlers often start many workers). \
Evidence, best first: similar_past_runs are measured and the closest ones matter most; notice when the same command used very different amounts in different folders or with different arguments. \
project shows what the command expands to (script_body, and script_file for the first lines of a wrapper script it runs), how big the repo is and which worker or concurrency settings are in force. \
machine and running_now show what is free. \
requester_estimate, when present, is the launching agent's own guess: it knows what it is about to do, but its numbers are rough, so use it as a clue and prefer measurements. \
For a command with no similar runs, judge from the tool and what it does. \
Answer with JSON only: {\"ram_mb\": integer, \"cores\": number}.";

fn build_prompt(
    events: &[Event],
    label: Option<&str>,
    cmd: &str,
    cwd: &str,
    running: &[SlotInfo],
    hint: &Hint,
    context: Value,
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
            } else if their_key.split_whitespace().next() == Some(first.as_str())
                && !first.is_empty()
            {
                score += 2;
            }
            if e.cwd.as_deref() == Some(cwd) {
                score += 1;
            }
            (score >= 2).then_some((score, e))
        })
        .collect();
    similar.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.ts.cmp(&a.1.ts)));
    let now = now_ms();
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
                "exit_code": e.exit,
                "days_ago": now.saturating_sub(e.ts) / 86_400_000,
            })
        })
        .collect();
    let others: Vec<Value> = running
        .iter()
        .map(|job| json!({"command": clip(&job.cmd, 100), "expected_ram_mb": job.est_peak_mb, "expected_cores": job.est_cores}))
        .collect();
    let mut prompt = json!({
        "command": clip(cmd, 400),
        "label": label,
        "cwd": cwd,
        "running_now": others,
        "similar_past_runs": runs,
    });
    if let (Some(map), Some(extra)) = (prompt.as_object_mut(), context.as_object()) {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
    if !hint.is_empty() {
        prompt["requester_estimate"] = json!({
            "peak_ram_mb": hint.ram_mb,
            "peak_cores": hint.cores,
            "seconds": hint.secs,
        });
    }
    prompt.to_string()
}

/// What the model cannot see from past runs: the machine right now and the project
/// the command runs in.
fn gather_context(cwd: &str, cmd: &str) -> Value {
    let mem = crate::sysinfo::memory();
    let round = |v: f64| (v * 10.0).round() / 10.0;
    let mut project = serde_json::Map::new();
    if let Some(body) = script_body(Path::new(cwd), cmd) {
        project.insert("script_body".into(), json!(clip(&body, 400)));
    }
    if let Some((name, head)) = script_head(Path::new(cwd), cmd) {
        project.insert(
            "script_file".into(),
            json!({"path": name, "first_lines": head}),
        );
    }
    let markers: Vec<&str> = [
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
        "Makefile",
        "playwright.config.ts",
        "vitest.config.ts",
        "docker-compose.yml",
    ]
    .into_iter()
    .filter(|f| Path::new(cwd).join(f).exists())
    .collect();
    project.insert("files_present".into(), json!(markers));
    if let Some(n) = tracked_files(cwd) {
        project.insert("git_tracked_files".into(), json!(n));
    }
    let tuning: serde_json::Map<String, Value> = std::env::vars()
        .filter(|(k, v)| {
            let k = k.to_uppercase();
            !v.is_empty()
                && v.len() < 20
                && ["WORKERS", "CONCURRENCY", "PARALLEL", "JOBS", "THREADS"]
                    .iter()
                    .any(|w| k.contains(w))
        })
        .map(|(k, v)| (k, json!(v)))
        .collect();
    if !tuning.is_empty() {
        project.insert("parallelism_settings".into(), Value::Object(tuning));
    }
    json!({
        "project": project,
        "machine": {
            "cores": crate::sysinfo::cores(),
            "total_ram_gb": crate::sysinfo::total_ram_gb().map(round),
            "free_ram_gb": mem.map(|m| round(m.avail_gb)),
            "swap_used_gb": mem.map(|m| round(m.swap_used_gb)),
            "load_1m": crate::sysinfo::load_average().map(round),
        },
    })
}

/// For `npm|pnpm|yarn|bun [run] <script>`: the script's command line from package.json.
fn script_body(cwd: &Path, cmd: &str) -> Option<String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| matches!(*w, "npm" | "pnpm" | "yarn" | "bun"))?;
    let mut rest = words[at + 1..].iter().filter(|w| !w.starts_with('-'));
    let mut name = *rest.next()?;
    if matches!(name, "run" | "run-script") {
        name = *rest.next()?;
    }
    cwd.ancestors().take(5).find_map(|dir| {
        let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let json: Value = serde_json::from_str(&text).ok()?;
        json.pointer("/scripts")?
            .get(name)?
            .as_str()
            .map(String::from)
    })
}

/// The first lines of a wrapper script the command runs (`bash scripts/ci.sh`,
/// `./test.sh`, or one named inside a package.json script). Comments and lines that
/// look like they hold secrets are dropped, since the text goes to the model.
fn script_head(cwd: &Path, cmd: &str) -> Option<(String, String)> {
    const SECRET: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];
    let mut text = without_env_prefix(cmd);
    if let Some(body) = script_body(cwd, cmd) {
        text.push(' ');
        text.push_str(&body);
    }
    let path = text.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c| c == '"' || c == '\'' || c == ';');
        let is_script = word.starts_with("./")
            || [".sh", ".py", ".js", ".mjs", ".ts", ".rb"]
                .iter()
                .any(|ext| word.ends_with(ext));
        if !is_script || word.starts_with('-') || word.contains('*') {
            return None;
        }
        let full = cwd.join(word);
        let meta = std::fs::metadata(&full).ok()?;
        (meta.is_file() && meta.len() < 200_000).then_some((word.to_string(), full))
    })?;
    let content = std::fs::read_to_string(&path.1).ok()?;
    let lines: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//"))
        .filter(|l| {
            let upper = l.to_uppercase();
            !SECRET.iter().any(|w| upper.contains(w))
        })
        .take(30)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some((path.0, clip(&lines.join("\n"), 800)))
}

fn tracked_files(cwd: &str) -> Option<usize> {
    let out = Command::new("git")
        .args(["-C", cwd, "ls-files"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| out.stdout.iter().filter(|b| **b == b'\n').count())
}

/// Drops a leading `env -u X NAME=value ...` so a long PATH does not hide the command.
fn without_env_prefix(cmd: &str) -> String {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let assignment = t.split_once('=').is_some_and(|(n, _)| {
            !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
        if t == "env" || assignment {
            i += 1;
        } else if t.starts_with('-') && i > 0 && tokens[..i].contains(&"env") {
            i += if t == "-u" { 2 } else { 1 };
        } else {
            break;
        }
    }
    if i >= tokens.len() {
        cmd.to_string()
    } else {
        tokens[i..].join(" ")
    }
}

fn clip(text: &str, max: usize) -> String {
    let one_line = without_env_prefix(text).replace('\n', " ; ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(max).collect();
    cut.push('…');
    cut
}

fn ask(model: &str, api_key: &str, prompt: &str, timeout_secs: u64) -> Option<(f64, f64)> {
    parse_answer(&ask_raw(
        model,
        api_key,
        SYSTEM_PROMPT,
        prompt,
        timeout_secs,
    )?)
}

/// One call to OpenRouter through curl, with the key passed on stdin so it never
/// shows in the process list. Returns the response body, errors included.
fn ask_raw(
    model: &str,
    api_key: &str,
    system: &str,
    prompt: &str,
    timeout_secs: u64,
) -> Option<String> {
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
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
            "-m",
            &timeout_secs.to_string(),
            "-K",
            "-",
            ENDPOINT,
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
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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

fn cache_path(root: &Path, key: &str, cwd: &str, hint: &Hint) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    (
        key,
        cwd,
        hint.ram_mb,
        hint.secs,
        hint.cores.map(|c| c.to_bits()),
    )
        .hash(&mut hasher);
    root.join("predict-cache")
        .join(format!("{:016x}.json", hasher.finish()))
}

/// An answer lasts until a job of the same kind finishes and gives a new
/// measurement (or a day passes), so a burst of the same command costs one call
/// and the next call after real data is made with that data.
fn read_cache(path: &Path, events: &[Event], key: &str) -> Option<Prediction> {
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let at = value.get("ts")?.as_u64()?;
    if now_ms().saturating_sub(at) > CACHE_TTL_MS {
        return None;
    }
    if events
        .iter()
        .any(|e| e.ev == "finish" && e.ts > at && stats::command_key(e) == key)
    {
        return None;
    }
    Some(Prediction {
        ram_mb: value.get("ram_mb")?.as_u64()?,
        cores: value.get("cores")?.as_f64()?,
        source: "cache",
        raw: None,
    })
}

fn write_cache(path: &Path, prediction: &Prediction) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json =
        json!({"ts": now_ms(), "ram_mb": prediction.ram_mb, "cores": prediction.cores}).to_string();
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

    fn finish(cmd: &str, cwd: &str, mb: u64, ts: u64) -> Event {
        Event {
            ts,
            ev: "finish".into(),
            cmd: Some(cmd.into()),
            cwd: Some(cwd.into()),
            peak_rss_mb: Some(mb),
            peak_cores: Some(3.0),
            ..Default::default()
        }
    }

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
        let events = vec![
            finish("cargo build", "/a", 111, 1),
            finish("bun run ship", "/a", 222, 2),
            finish("bun run ship", "/b", 333, 3),
            finish("bun run format", "/b", 444, 4),
        ];
        let prompt = build_prompt(
            &events,
            None,
            "bun run ship",
            "/a",
            &[],
            &Hint::default(),
            json!({}),
        );
        let at = |needle: &str| prompt.find(needle);
        assert!(at("222").unwrap() < at("333").unwrap());
        assert!(at("333").unwrap() < at("444").unwrap());
        assert!(at("111").is_none());
        assert!(at("requester_estimate").is_none());
    }

    #[test]
    fn the_requesters_own_estimate_goes_into_the_prompt() {
        let hint = Hint {
            ram_mb: Some(900),
            cores: None,
            secs: Some(120),
        };
        let prompt = build_prompt(
            &[],
            None,
            "pytest",
            "/a",
            &[],
            &hint,
            json!({"machine": {"cores": 8}}),
        );
        let value: Value = serde_json::from_str(&prompt).unwrap();
        assert_eq!(value["requester_estimate"]["peak_ram_mb"], 900);
        assert_eq!(value["requester_estimate"]["seconds"], 120);
        assert_eq!(value["machine"]["cores"], 8);
    }

    #[test]
    fn parses_sizes_and_durations_agents_would_write() {
        assert_eq!(parse_mb("4gb"), Some(4096));
        assert_eq!(parse_mb("800MB"), Some(800));
        assert_eq!(parse_mb("1.5g"), Some(1536));
        assert_eq!(parse_mb("700"), Some(700));
        assert_eq!(parse_mb("lots"), None);
        assert_eq!(parse_secs("90s"), Some(90));
        assert_eq!(parse_secs("3m"), Some(180));
        assert_eq!(parse_secs("2h"), Some(7200));
        assert_eq!(parse_secs("45"), Some(45));
        assert_eq!(parse_secs("soon"), None);
    }

    #[test]
    fn the_floor_is_strict_in_the_same_folder_and_loose_elsewhere() {
        let mut events: Vec<Event> = [400, 5000, 5000, 5000, 5000]
            .iter()
            .map(|mb| finish("bun test", "/big", *mb, 1))
            .collect();
        events.push(finish("bun test", "/small", 400, 2));
        assert_eq!(history_floor(&events, "bun test", "/big").unwrap().0, 5000);
        assert!(history_floor(&events, "bun test", "/small").unwrap().0 <= 5000);
        let elsewhere = history_floor(&events, "bun test", "/new").unwrap().0;
        assert!(
            elsewhere < 5000,
            "a new folder is only held to the lowest quarter, got {elsewhere}"
        );
        assert!(history_floor(&events[..2], "bun test", "/new").is_none());
    }

    #[test]
    fn a_cached_answer_expires_when_a_new_measurement_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let hit = Prediction {
            ram_mb: 1000,
            cores: 2.0,
            source: "model",
            raw: None,
        };
        write_cache(&path, &hit);
        assert_eq!(read_cache(&path, &[], "bun test").unwrap().source, "cache");
        let newer = finish("bun test", "/a", 900, now_ms() + 5_000);
        assert!(read_cache(&path, &[newer], "bun test").is_none());
        let other = finish("cargo test", "/a", 900, now_ms() + 5_000);
        assert!(read_cache(&path, &[other], "bun test").is_some());
    }

    #[test]
    fn finds_the_script_a_package_runner_will_execute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"ship": "bun run build && bun run test"}}"#,
        )
        .unwrap();
        assert_eq!(
            script_body(dir.path(), "env -u X bun run ship --fast").as_deref(),
            Some("bun run build && bun run test")
        );
        assert_eq!(script_body(dir.path(), "bun run nope"), None);
    }

    #[test]
    fn reads_the_head_of_a_wrapper_script_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(
            dir.path().join("scripts/ci.sh"),
            "#!/bin/bash\n# comment\nexport API_TOKEN=abc\nset -e\nplaywright test --workers=8\n",
        )
        .unwrap();
        let (name, head) = script_head(dir.path(), "bash scripts/ci.sh").unwrap();
        assert_eq!(name, "scripts/ci.sh");
        assert!(head.contains("playwright test --workers=8"));
        assert!(!head.contains("abc") && !head.contains("comment"));
        assert!(script_head(dir.path(), "bash scripts/missing.sh").is_none());
    }

    #[test]
    fn env_noise_in_front_of_a_command_is_dropped() {
        assert_eq!(
            without_env_prefix("env -u FORCE_COLOR NO_COLOR=1 PATH=/a:/b bun run ship"),
            "bun run ship"
        );
        assert_eq!(
            without_env_prefix("bun run ship --flag=1"),
            "bun run ship --flag=1"
        );
        assert_eq!(without_env_prefix("env A=1"), "env A=1");
    }

    #[test]
    fn long_multiline_commands_are_clipped_to_one_line() {
        let clipped = clip("set -e\ncd x\nbun run test", 10);
        assert!(!clipped.contains('\n'));
        assert!(clipped.ends_with('…'));
    }
}
