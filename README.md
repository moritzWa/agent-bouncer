# agent-bouncer

A bouncer for your CPU and RAM. Many coding agents run builds, test suites and
evals on one machine. This lets only as many in as the machine can take. The
rest wait in line, first come, first served.

```text
$ agent-bouncer status
slots 2/3 in use   free RAM 9.4 GB   swap 3.7 GB   CPU 41%

running
  slot 0   4m12s   2.1 GB   cargo test            ~/Code/agentview
  slot 1   0m48s   5.3 GB   pnpm playwright test  ~/Code/arca

waiting
  1.  0m31s   cargo clippy   ~/Code/agentview   (all slots are in use)
```

No daemon. It uses file locks and a small state folder, so nothing runs in the
background and a crashed job can never leak a slot.

## Use

```sh
agent-bouncer run --wait -- cargo test
agent-bouncer run --wait --weight 2 -- pnpm playwright test   # takes two slots
agent-bouncer run --max-wait 600 --label evals -- ./run-evals.sh
```

Without `--wait` it exits 75 straight away when busy. With `--wait` it queues.
`--max-wait SECONDS` gives up with 75 after that long (default 1800, 0 means
never).

```sh
agent-bouncer status          # slots, machine load, running jobs, the line
agent-bouncer stats --since 7d  # waits, refusals, heaviest commands
agent-bouncer log -n 50       # raw events as JSON lines
```

## What it does

- **First come, first served.** Waiters are admitted in arrival order. A waiter
  that dies leaves the line on its own.
- **Admission by what the machine has.** A job starts only when a slot is free,
  free RAM is above a floor, and the CPU is not saturated. Both are measured
  now, not from a one-minute load average that stays high after a job ends.
- **Cleans up after jobs.** Each job runs in its own process group. When it ends,
  whatever it left running (dev servers, headless browsers) is stopped.
  `--no-reap` turns that off.
- **Never kills a running job.** When RAM runs low it stops admitting new jobs. It
  does not kill or requeue one that is already running.
- **Measures itself.** Every wait, refusal and run goes into an event log, so
  "how often am I resource-bound?" has a number. `stats` gives you waits per day,
  what blocked them, and each command's peak RAM.
- **No deadlocks.** A job that calls `agent-bouncer` itself runs directly.

## Settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGENT_BOUNCER_SLOTS` | cores / 6, between 2 and 6 | Heavy jobs at once |
| `AGENT_BOUNCER_MIN_FREE_GB` | 3 | Free RAM floor for admitting a job |
| `AGENT_BOUNCER_MAX_CPU` | 95 | Busy CPU percent above which no job is admitted (100 turns the check off) |
| `AGENT_BOUNCER_DIR` | `~/.local/state/agent-bouncer` | State and event log |

Exit codes: the job's own, 75 busy or wait limit passed, 64 bad usage, 127 cannot
run the command, 128+N when stopped by signal N.

## Install

```sh
cargo install --path .
```

macOS and Linux.

## License

MIT
