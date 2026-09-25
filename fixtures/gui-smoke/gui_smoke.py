#!/usr/bin/env python3
"""GUI smoke test (plan §6 L6, verification debt D3).

Drives the real desktop app through tauri-driver + WebKitWebDriver in a
virtual display, with a throwaway HOME (nothing lands in your Downloads):

    python3 fixtures/gui-smoke/gui_smoke.py <turbomerger-gui> <scratch dir> [--big <tree>]

(--big: a large tree to cancel, e.g. fixtures/big-tree; default: 15,000 small
files generated here.)

Run it under `xvfb-run -a`. Needs `WebKitWebDriver` (apt: webkit2gtk-driver),
`tauri-driver` (cargo install tauri-driver --locked) and `selenium` (pip).
Build the app with embedded assets first: `npx tauri build --debug --no-bundle`.

Checks, each with a screenshot in <scratch>/gui/:
 1. merge a folder: stats, the not-captured warning, redaction count;
 2. Scan & curate: the Skipped tab lists "Not captured" first;
 3. apply-back on the poisoning fixture: .git and symlinked targets blocked,
    control files and manifests held for confirmation; confirming build.rs
    applies it with src/main.rs; the hook and the outside file are untouched;
    Restore undoes it;
 4. cancel a long merge: acknowledged within 1 s, nothing is written;
 5. watch mode: an edit re-merges the stable output; stopping works.
"""

import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.options import ArgOptions
from selenium.webdriver.support.ui import WebDriverWait

REPO = Path(__file__).resolve().parents[2]
# 4444 is a common proxy port (I2P); keep tauri-driver off it.
PORT = int(os.environ.get("TM_WEBDRIVER_PORT", "4460"))
results: list = []


def check(name: str, ok: bool, detail: str = ""):
    results.append((name, ok, detail))
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f" — {detail}" if detail else ""), flush=True)


def wait_port(port: int, timeout: float = 20):
    end = time.time() + timeout
    while time.time() < end:
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.2)
    raise RuntimeError(f"port {port} never opened")


def build_repo(root: Path):
    (root / "src").mkdir(parents=True)
    (root / "src/main.rs").write_text("fn main() {\n    println!(\"hi\");\n}\n")
    (root / "README.md").write_text("# gui smoke\n")
    (root / "config.py").write_text('DB_PASSWORD = "Zq8vN3kL5pX2rT7w"\n')
    (root / "paper.pdf").write_bytes(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    (root / ".gitignore").write_text("build/\n")
    (root / "build").mkdir()
    (root / "build/out.txt").write_text("generated\n")


def main():
    app = Path(sys.argv[1]).resolve()
    sp = Path(sys.argv[2]).resolve()
    big = Path(sys.argv[sys.argv.index("--big") + 1]).resolve() if "--big" in sys.argv else None
    out = sp / "gui"
    shutil.rmtree(out, ignore_errors=True)
    home = out / "home"
    downloads = home / "Downloads"
    (home / ".config").mkdir(parents=True)
    downloads.mkdir(parents=True)
    (home / ".config/user-dirs.dirs").write_text(f'XDG_DOWNLOAD_DIR="{downloads}"\n')
    repo = out / "repo"
    build_repo(repo)
    subprocess.run(["bash", str(REPO / "fixtures/poison-applyback/build.sh"), str(out / "poison")],
                   check=True, capture_output=True)
    poison = out / "poison" / "root"
    hook = poison / ".git/hooks/pre-commit"
    hook_before = hook.read_bytes()
    outside_before = (out / "poison/outside/target.txt").read_bytes()

    env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_DATA_HOME=str(home / ".local/share"), XDG_CACHE_HOME=str(home / ".cache"),
               TURBOMERGER_STATE_DIR=str(home / "state"))
    # Its own process group: tauri-driver's WebKitWebDriver child goes with it.
    driver_proc = subprocess.Popen(["tauri-driver", "--port", str(PORT), "--native-port", str(PORT + 1)], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
    try:
        wait_port(PORT)
        opts = ArgOptions()
        opts.set_capability("browserName", "wry")
        opts.set_capability("tauri:options", {"application": str(app)})
        d = webdriver.Remote(command_executor=f"http://127.0.0.1:{PORT}", options=opts)
        try:
            run_checks(d, out, repo, poison, hook, hook_before, outside_before, downloads, big)
        finally:
            d.quit()
    finally:
        import signal
        os.killpg(driver_proc.pid, signal.SIGTERM)
        driver_proc.wait(timeout=10)
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    sys.exit(1 if failed else 0)


def txt(el) -> str:
    """Text of an element even when it is scrolled out of view (WebKit's
    WebDriver reports rendered text only for what is on screen)."""
    return (el.get_attribute("textContent") or "").strip()


def run_checks(d, out, repo, poison, hook, hook_before, outside_before, downloads, big):
    wait = WebDriverWait(d, 60)
    shot = lambda name: d.save_screenshot(str(out / f"{name}.png"))
    by_text = lambda tag, text: f"//{tag}[contains(normalize-space(.), '{text}')]"

    wait.until(lambda d: txt(d.find_element(By.CSS_SELECTOR, "h1.title")) == "TurboMerger")
    check("app starts", True)

    source = d.find_element(By.CSS_SELECTOR, "input[placeholder^='Folder']")

    def set_source(path: Path):
        source.clear()
        source.send_keys(str(path))

    # 1. Merge a folder.
    set_source(repo)
    d.find_element(By.CSS_SELECTOR, ".merge-main").click()
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".result-section"))
    shot("1-merge")
    stats = [txt(e) for e in d.find_elements(By.CSS_SELECTOR, ".stat-value")]
    breakdown = " | ".join(txt(e) for e in d.find_elements(By.CSS_SELECTOR, ".detection-breakdown"))
    check("merge shows results", stats[:1] == ["4"], f"stats={stats}")
    check("not-captured warning shown", "not captured" in breakdown, breakdown[:200])
    check("redaction count shown", "secret redacted" in breakdown or "secrets redacted" in breakdown)
    outputs = sorted(downloads.glob("repo_*_merged.md"))
    check("output written to the session Downloads", len(outputs) == 1, str(outputs))
    if outputs:
        text = outputs[0].read_text()
        check("secret not in the output", "Zq8vN3kL5pX2rT7w" not in text)

    # 2. Scan & curate.
    d.find_element(By.XPATH, by_text("button", "Scan & curate")).click()
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".curate-panel"))
    d.find_element(By.XPATH, "//button[contains(@class,'tab') and starts-with(normalize-space(.), 'Skipped')]").click()
    heads = [txt(e) for e in d.find_elements(By.CSS_SELECTOR, ".skip-group-head")]
    shot("2-curate")
    check("curate lists Not captured first", bool(heads) and "Not captured" in heads[0], f"{heads}")
    d.find_element(By.XPATH, by_text("button", "Done")).click()

    # 3. Apply-back on the poisoning fixture.
    set_source(poison)
    d.find_element(By.XPATH, by_text("button", "Apply an LLM reply")).click()
    area = wait.until(lambda d: d.find_element(By.CSS_SELECTOR, ".apply-textarea"))
    reply = (REPO / "fixtures/poison-applyback/reply.md").read_text()
    d.execute_script(
        "const t = arguments[0];"
        "const set = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;"
        "set.call(t, arguments[1]); t.dispatchEvent(new Event('input', { bubbles: true }));",
        area, reply)
    d.find_element(By.XPATH, by_text("button", "Preview changes")).click()
    wait.until(lambda d: len(d.find_elements(By.CSS_SELECTOR, ".apply-card")) >= 8)
    cards = {txt(c.find_element(By.CSS_SELECTOR, ".apply-path")): c
             for c in d.find_elements(By.CSS_SELECTOR, ".apply-card")}
    shot("3-apply-preview")
    blocked = [p for p, c in cards.items() if c.find_elements(By.CSS_SELECTOR, ".badge-blocked")]
    held = [p for p, c in cards.items() if c.find_elements(By.CSS_SELECTOR, ".apply-confirm")]
    check(".git and the symlinked target are blocked",
          {".git/hooks/pre-commit", ".GIT/hooks/post-checkout", "docs/note.md"} <= set(blocked), f"{blocked}")
    check("control files and manifests are held", {"build.rs", ".github/workflows/ci.yml"} <= set(held), f"{held}")
    # Confirming a held file also ticks it for apply.
    cards["build.rs"].find_element(By.CSS_SELECTOR, ".apply-confirm input").click()
    d.find_element(By.XPATH, by_text("button", "Apply 2 of")).click()
    ok = wait.until(lambda d: d.find_element(By.CSS_SELECTOR, ".apply-ok"))
    shot("3-apply-done")
    check("confirmed manifest applied with main.rs", "Applied 2 files" in txt(ok), txt(ok))
    check("build.rs written", (poison / "build.rs").exists())
    check("the git hook is untouched", hook.read_bytes() == hook_before)
    check("the file outside the root is untouched",
          (out / "poison/outside/target.txt").read_bytes() == outside_before)
    d.find_element(By.XPATH, "//div[contains(@class,'apply-result')]//button[normalize-space(.)='Restore']").click()
    wait.until(lambda d: not (poison / "build.rs").exists())
    check("restore undoes the apply",
          not (poison / "build.rs").exists() and (poison / "src/main.rs").read_text() == "fn main() {}\n")
    d.find_element(By.XPATH, by_text("button", "Apply an LLM reply")).click()

    # 4. Cancel a long merge.
    if not (big and big.is_dir()):
        big = out / "cancel-corpus"
        for i in range(15_000):
            sub = big / f"d{i // 500:02d}"
            sub.mkdir(parents=True, exist_ok=True)
            (sub / f"f{i:05d}.py").write_text(f"def f{i}():\n    return '{'x' * 1500}'\n")
    if big.is_dir():
        set_source(big)
        before = set(downloads.iterdir())
        d.find_element(By.CSS_SELECTOR, ".merge-main").click()
        wait.until(lambda d: "files" in " ".join(txt(e) for e in d.find_elements(By.CSS_SELECTOR, ".progress-text")))
        shot("4-progress")
        t0 = time.time()
        d.find_element(By.XPATH, by_text("button", "CANCEL")).click()
        WebDriverWait(d, 30).until(lambda d: d.find_elements(By.CSS_SELECTOR, ".cancelled-text"))
        latency = time.time() - t0
        shot("4-cancelled")
        check("cancel is acknowledged within 1 s", latency < 1.0, f"{latency:.2f} s")
        time.sleep(0.5)
        check("a cancelled merge writes nothing", set(downloads.iterdir()) == before,
              f"{sorted(set(downloads.iterdir()) - before)}")

    # 5. Watch mode.
    set_source(repo)
    d.find_element(By.XPATH, "//button[normalize-space(.)='Watch']").click()
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".watch-banner"))
    watch_out = downloads / "repo_watch_merged.md"
    wait.until(lambda d: watch_out.exists())
    (repo / "src/lib.rs").write_text("pub fn watched_change() {}\n")
    try:
        WebDriverWait(d, 20).until(lambda d: "watched_change" in watch_out.read_text())
        check("watch re-merges on change", True)
    except Exception:
        check("watch re-merges on change", False, "output never updated")
    shot("5-watch")
    d.find_element(By.XPATH, by_text("button", "Watching")).click()
    wait.until(lambda d: not d.find_elements(By.CSS_SELECTOR, ".watch-banner"))
    check("watch stops", True)


if __name__ == "__main__":
    main()
