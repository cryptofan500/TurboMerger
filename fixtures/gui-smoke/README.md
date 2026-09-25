# gui-smoke — the desktop app, clicked through (plan §6 L6, debt D3)

`gui_smoke.py` drives `turbomerger-gui` through `tauri-driver` and WebKit's
WebDriver inside a virtual display, with a throwaway `HOME` (outputs land in
`<scratch>/gui/home/Downloads`, never in yours). 18 checks: merge + warnings,
Scan & curate, the apply-back confirmation flow on the poisoning fixture and
Restore, cancel latency, watch mode. Screenshots go to `<scratch>/gui/`.

```bash
sudo apt-get install -y webkit2gtk-driver xvfb      # Debian/Ubuntu
cargo install tauri-driver --locked
python3 -m venv .venv && .venv/bin/pip install selenium
npx tauri build --debug --no-bundle                 # embeds the frontend
xvfb-run -a .venv/bin/python fixtures/gui-smoke/gui_smoke.py target/debug/turbomerger-gui "$SP"
```

`TM_WEBDRIVER_PORT` (default 4460) moves tauri-driver off a busy port; 4444 is
often an I2P proxy. CI runs this on Linux after the debug app build.
