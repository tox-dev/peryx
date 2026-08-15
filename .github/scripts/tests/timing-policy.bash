#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

write() {
  local root=$1
  local path=$2
  local content=$3
  mkdir -p "$(dirname "$root/$path")"
  printf '%s\n' "$content" >"$root/$path"
}

accept="$scratch/accept"
write "$accept" crates/owner/tests/unit/virtual.rs $'#[tokio::test(start_paused = true)]\nasync fn advances_time() {\n    tokio::time::sleep(Duration::from_secs(1)).await;\n    tokio::time::advance(Duration::from_secs(1)).await;\n    assert_eq!(tokio::time::Instant::now().elapsed(), Duration::ZERO);\n}'
write "$accept" crates/owner/tests/system/boundary.rs $'async fn boundary() {\n    tokio::time::timeout(LIMIT, receiver.recv()).await.unwrap();\n}'
write "$accept" crates/owner/tests/unit/finite.rs $'fn validates(values: &[u8]) {\n    for value in values { assert!(*value > 0); }\n}'
write "$accept" crates/owner/tests/unit/clock_input.rs $'fn classifies() {\n    assert_eq!(tracker.suspicion(Instant::now()), Suspicion::Unknown);\n}'
write "$accept" crates/owner/tests/unit/fixture.rs \
  'const FIXTURE: &str = env!("CARGO_BIN_EXE_owner-fixture");'
write "$accept" crates/owner/src/bench/drain.rs \
  'async fn drain(mut response: reqwest::Response) { while response.chunk().await?.is_some() {} }'
write "$accept" crates/owner/src/bench/pacing.rs $'async fn pace(interval: Duration, window: Duration) {\n    let start = TokioInstant::now();\n    let deadline = start + window;\n    let mut intended = start;\n    while TokioInstant::now() < deadline {\n        intended += interval;\n        sleep_until(intended).await;\n        request().await;\n    }\n}'
write "$accept" crates/owner/tests/system/readiness.sh $'timeout 5 docker compose wait service'
write "$accept" tests/frontend/boundary.mjs $'await Promise.race([request, boundaryTimeout]);'
write "$accept" tests/frontend/response.mjs $'await page.waitForResponse(isComplete);\nawait expect(page.locator("[data-ready]")).toBeVisible();'
write "$accept" tests/frontend/race-timeout.mjs $'await Promise.race([request, new Promise((resolve) => setTimeout(resolve, 1000))]);'
write "$accept" tests/frontend/process-timeout.mjs $'const startupTimeout = setTimeout(fail, 1000);\nawait started.finally(() => clearTimeout(startupTimeout));\nsetTimeout(forceExit, 1000).unref();'
"$repo/.github/scripts/check-test-timing" "$accept"

reject() {
  local name=$1
  local path=$2
  local content=$3
  local expected=$4
  local root="$scratch/$name"
  local output
  write "$root" "$path" "$content"
  if output=$("$repo/.github/scripts/check-test-timing" "$root" 2>&1); then
    printf '%s was accepted\n' "$name" >&2
    exit 1
  fi
  if [[ $output != *"$expected"* ]]; then
    printf '%s reported the wrong failure:\n%s\n' "$name" "$output" >&2
    exit 1
  fi
  first_line=${output%%$'\n'*}
  if [[ $first_line != "$path:"[0-9]*": "* ]]; then
    printf '%s omitted its source location:\n%s\n' "$name" "$output" >&2
    exit 1
  fi
}

reject rust-sleep crates/owner/tests/unit/sleep.rs \
  "$(printf 'fn test() { std::thread::%s(Duration::from_millis(1)); }' sleep)" \
  'blind sleep'
reject nested-owner-yield crates/owner/tests/system-suite/tests/cases/poll.rs \
  $'async fn poll() {\n    while !ready() { tokio::task::yield_now().await; }\n}' \
  'yield-based polling loop'
reject rust-bounded-yield crates/owner/tests/system/bounded_poll.rs \
  $'async fn boundary() {\n    tokio::time::timeout(LIMIT, async {\n        while !ready() { tokio::task::yield_now().await; }\n    }).await.unwrap();\n}' \
  'yield-based polling loop'
reject rust-interval crates/owner/tests/system/interval.rs \
  $'async fn boundary() {\n    tokio::time::timeout(LIMIT, async {\n        let mut ticker = tokio::time::interval(STEP);\n        loop { ticker.tick().await; }\n    }).await.unwrap();\n}' \
  'interval polling'
reject rust-spin crates/owner/tests/unit/spin.rs \
  $'fn poll() {\n    loop { std::hint::spin_loop(); }\n}' \
  'unbounded spin loop'
reject rust-try-poll crates/owner/tests/unit/try_poll.rs \
  $'fn poll() {\n    while receiver.try_recv().is_err() {}\n}' \
  'try-poll loop'
reject rust-empty-loop crates/owner/tests/unit/empty.rs \
  'fn poll() { loop {} }' \
  'unbounded empty loop'
reject rust-current-exe crates/owner/tests/unit/self_exec.rs \
  'fn fixture() { std::env::current_exe().unwrap(); }' \
  'test self-execution'
reject rust-benchmark-current-exe crates/owner/src/bench/self_exec.rs \
  'fn fixture() { std::env::current_exe().unwrap(); }' \
  'test self-execution'
reject rust-benchmark-source crates/owner/src/bench/server.rs \
  "$(printf 'fn fixture() { std::thread::%s(Duration::from_millis(1)); }' sleep)" \
  'blind sleep'
reject rust-benchmark-sleep-until crates/owner/src/bench/server.rs \
  'async fn fixture(deadline: Instant) { sleep_until(deadline).await; }' \
  'blind sleep'
reject rust-non-response-empty-loop crates/owner/tests/unit/stream.rs \
  'async fn drain(mut stream: Stream) { while stream.next().await.is_some() {} }' \
  'unbounded empty loop'
reject wall-clock crates/owner/tests/unit/duration.rs \
  $'fn test() {\n    let start = Instant::now();\n    assert!(start.elapsed() < Duration::from_secs(1));\n}' \
  'wall-clock assertion'
reject shell-sleep crates/owner/fuzz/fixtures/start.sh \
  "$(printf '%s 1' sleep)" \
  'blind sleep'
reject embedded-shell crates/owner/benches/fixtures/server.rs \
  "$(printf 'const SCRIPT: &str = \"while [ ! -e ready ]; do %s; done\";' :)" \
  'busy or sleeping embedded shell'
reject javascript-delay crates/owner/tests/frontend/delay.mjs \
  'await page.waitForTimeout(100);' \
  'blind delay'
reject javascript-clock tests/frontend/duration.mjs \
  'expect(Date.now() - started).toBeLessThan(100);' \
  'wall-clock assertion'
reject javascript-busy tests/frontend/poll.mjs \
  'while (true) {}' \
  'unbounded busy poll'
reject javascript-yield tests/frontend/yield.mjs \
  $'while (!ready) {\n  await scheduler.yield();\n}' \
  'unbounded busy poll'
reject javascript-expect-poll tests/frontend/expect-poll.mjs \
  'await expect.poll(() => status()).toBe("ready");' \
  'expect.poll retries'

multiple="$scratch/multiple"
write "$multiple" crates/owner/tests/unit/first.rs \
  "$(printf 'fn test() { std::thread::%s(Duration::from_millis(1)); }' sleep)"
write "$multiple" tests/frontend/second.mjs 'while (true) {}'
if output=$("$repo/.github/scripts/check-test-timing" "$multiple" 2>&1); then
  printf 'multiple violations were accepted\n' >&2
  exit 1
fi
for expected in \
  'crates/owner/tests/unit/first.rs:1: blind sleep' \
  'tests/frontend/second.mjs:1: unbounded busy poll'; do
  if [[ $output != *"$expected"* ]]; then
    printf 'multiple violations omitted %s:\n%s\n' "$expected" "$output" >&2
    exit 1
  fi
done
