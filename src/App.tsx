import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import "./styles/global.css";

interface MergeResult {
  output_path: string;
  output_paths: string[];
  files_processed: number;
  files_skipped: number;
  total_bytes: number;
  duration_ms: number;
  files_by_extension: number;
  files_by_content: number;
  files_skipped_binary: number;
  files_unreadable: number;
  secrets_redacted: number;
  tokens_o200k: number;
  tokens_claude_est: number;
}

interface ProgressUpdate {
  current: number;
  total: number;
  current_file: string;
  percentage: number;
}

type Status = "ready" | "scanning" | "merging" | "done" | "error" | "cancelled";
type Preset = "custom" | "lean" | "archive" | "docs" | "claude";

const SETTINGS_KEY = "turbomerger.settings.v1";

function App() {
  const [status, setStatus] = useState<Status>("ready");
  const [version, setVersion] = useState<string>("");
  const [sourcePath, setSourcePath] = useState<string>("");
  const [outputPath, setOutputPath] = useState<string>("");

  const [includeVenv, setIncludeVenv] = useState(false);
  const [includeTree, setIncludeTree] = useState(true);
  const [contentDetection, setContentDetection] = useState(true);
  const [respectGitignore, setRespectGitignore] = useState(true);
  const [includeHidden, setIncludeHidden] = useState(false);
  const [redactSecrets, setRedactSecrets] = useState(true);
  const [format, setFormat] = useState<string>("markdown");
  const [ordering, setOrdering] = useState<string>("path");
  const [maxTokens, setMaxTokens] = useState<string>("");
  const [includeGlobs, setIncludeGlobs] = useState<string>("");
  const [excludeGlobs, setExcludeGlobs] = useState<string>("");
  const [removeEmptyLines, setRemoveEmptyLines] = useState(false);
  const [truncateBase64, setTruncateBase64] = useState(false);
  const [preset, setPreset] = useState<Preset>("custom");
  const [showAdvanced, setShowAdvanced] = useState(false);

  const [result, setResult] = useState<MergeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<ProgressUpdate | null>(null);

  // Load persisted settings + version on mount.
  useEffect(() => {
    invoke<string>("get_downloads_path").then(setOutputPath).catch(console.error);
    getVersion().then(setVersion).catch(console.error);
    try {
      const raw = localStorage.getItem(SETTINGS_KEY);
      if (raw) {
        const s = JSON.parse(raw);
        if (typeof s.includeVenv === "boolean") setIncludeVenv(s.includeVenv);
        if (typeof s.includeTree === "boolean") setIncludeTree(s.includeTree);
        if (typeof s.contentDetection === "boolean") setContentDetection(s.contentDetection);
        if (typeof s.respectGitignore === "boolean") setRespectGitignore(s.respectGitignore);
        if (typeof s.includeHidden === "boolean") setIncludeHidden(s.includeHidden);
        if (typeof s.redactSecrets === "boolean") setRedactSecrets(s.redactSecrets);
        if (typeof s.format === "string") setFormat(s.format);
        if (typeof s.ordering === "string") setOrdering(s.ordering);
        if (typeof s.maxTokens === "string") setMaxTokens(s.maxTokens);
        if (typeof s.includeGlobs === "string") setIncludeGlobs(s.includeGlobs);
        if (typeof s.excludeGlobs === "string") setExcludeGlobs(s.excludeGlobs);
        if (typeof s.removeEmptyLines === "boolean") setRemoveEmptyLines(s.removeEmptyLines);
        if (typeof s.truncateBase64 === "boolean") setTruncateBase64(s.truncateBase64);
        if (typeof s.sourcePath === "string") setSourcePath(s.sourcePath);
      }
    } catch { /* ignore corrupt settings */ }
  }, []);

  // Persist settings on change.
  useEffect(() => {
    const s = {
      includeVenv, includeTree, contentDetection, respectGitignore, includeHidden,
      redactSecrets, format, ordering, maxTokens, includeGlobs, excludeGlobs,
      removeEmptyLines, truncateBase64, sourcePath,
    };
    try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(s)); } catch { /* quota */ }
  }, [includeVenv, includeTree, contentDetection, respectGitignore, includeHidden,
      redactSecrets, format, ordering, maxTokens, includeGlobs, excludeGlobs,
      removeEmptyLines, truncateBase64, sourcePath]);

  // Progress listener.
  useEffect(() => {
    const unlisten = listen<ProgressUpdate>("merge-progress", (event) => {
      setProgress(event.payload);
      if (event.payload.total > 0) setStatus("merging");
    });
    return () => { unlisten.then((f) => f()); };
  }, []);

  // Drag-and-drop a folder onto the window.
  useEffect(() => {
    const p = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "drop" && event.payload.paths.length > 0) {
        setSourcePath(event.payload.paths[0]);
      }
    });
    return () => { p.then((f) => f()); };
  }, []);

  const applyPreset = (p: Preset) => {
    setPreset(p);
    if (p === "lean") {
      setRespectGitignore(true); setRedactSecrets(true); setIncludeTree(true);
      setFormat("markdown"); setOrdering("entry-first"); setMaxTokens("180000");
      setExcludeGlobs("**/*.lock,**/*.min.*"); setRemoveEmptyLines(false); setTruncateBase64(true);
    } else if (p === "claude") {
      setRespectGitignore(true); setRedactSecrets(true); setIncludeTree(true);
      setFormat("cxml"); setOrdering("important-last"); setMaxTokens("180000");
      setTruncateBase64(true);
    } else if (p === "archive") {
      setRespectGitignore(false); setRedactSecrets(true); setIncludeTree(true);
      setIncludeHidden(true); setIncludeVenv(false); setFormat("markdown");
      setOrdering("path"); setMaxTokens(""); setExcludeGlobs(""); setIncludeGlobs("");
    } else if (p === "docs") {
      setRespectGitignore(true); setFormat("markdown"); setOrdering("path");
      setIncludeGlobs("**/*.md,**/*.mdx,**/*.rst,**/*.txt"); setExcludeGlobs("");
    }
  };

  const selectSource = async () => {
    const selected = await open({ directory: true, multiple: false, title: "Select Codebase to Merge" });
    if (selected && typeof selected === "string") setSourcePath(selected);
  };

  const selectOutput = async () => {
    const selected = await save({
      defaultPath: outputPath,
      filters: [{ name: "Output", extensions: ["md", "xml", "json", "txt"] }],
      title: "Save Merged Output As",
    });
    if (selected) setOutputPath(selected);
  };

  const handleCancel = async () => {
    await invoke("cancel_merge");
    setStatus("cancelled");
    setProgress(null);
  };

  const handleMerge = async () => {
    if (!sourcePath) return;
    try {
      await invoke("reset_cancel");
      setStatus("scanning");
      setError(null);
      setResult(null);
      setProgress(null);

      const toList = (s: string) => s.split(/[,\n]/).map((x) => x.trim()).filter(Boolean);
      const options = {
        folder_path: sourcePath,
        output_path: outputPath || null,
        include_venv: includeVenv,
        include_tree: includeTree,
        content_detection: contentDetection,
        respect_gitignore: respectGitignore,
        include_hidden: includeHidden,
        redact_secrets: redactSecrets,
        format,
        ordering,
        max_tokens: maxTokens ? parseInt(maxTokens, 10) || 0 : 0,
        include_globs: toList(includeGlobs),
        exclude_globs: toList(excludeGlobs),
        remove_empty_lines: removeEmptyLines,
        truncate_base64: truncateBase64,
      };

      const mergeResult = await invoke<MergeResult>("merge_folder", { options });
      setResult(mergeResult);
      setStatus("done");
    } catch (err) {
      const msg = String(err);
      if (msg.includes("cancelled")) setStatus("cancelled");
      else { setError(msg); setStatus("error"); }
    }
  };

  const openThing = (path: string) => invoke("open_file", { path }).catch(console.error);
  const openFolder = (path: string) => invoke("open_folder", { path }).catch(console.error);

  const formatBytes = (b: number) =>
    b < 1024 ? `${b} B` : b < 1048576 ? `${(b / 1024).toFixed(1)} KB` : `${(b / 1048576).toFixed(1)} MB`;
  const formatDuration = (ms: number) =>
    ms < 1000 ? `${ms}ms` : ms < 60000 ? `${(ms / 1000).toFixed(1)}s` : `${(ms / 60000).toFixed(1)}m`;
  const formatTokens = (n: number) =>
    n < 1000 ? `${n}` : n < 1e6 ? `${(n / 1000).toFixed(0)}k` : `${(n / 1e6).toFixed(2)}M`;
  const fitHint = (claude: number) =>
    claude <= 128000 ? "fits GPT 128k & Claude 200k"
    : claude <= 200000 ? "fits Claude 200k; over GPT 128k"
    : claude <= 1000000 ? "over Claude 200k — split or use Gemini 1M"
    : "over 1M — splitting required";

  const isWorking = status === "scanning" || status === "merging";

  return (
    <div className="app">
      <div className="container">
        <header className="header">
          <h1 className="title">TurboMerger</h1>
          <span className="version">{version ? `v${version}` : ""}</span>
        </header>

        <section className="section">
          <label className="label">SOURCE CODEBASE</label>
          <div className="input-row">
            <input type="text" className="input" value={sourcePath} readOnly placeholder="Select or drag a folder here..." />
            <button className="btn btn-secondary" onClick={selectSource} disabled={isWorking}>Browse</button>
          </div>
        </section>

        <section className="section">
          <label className="label">OUTPUT DESTINATION</label>
          <div className="input-row">
            <input type="text" className="input" value={outputPath} readOnly placeholder="Output folder or file..." />
            <button className="btn btn-secondary" onClick={selectOutput} disabled={isWorking}>Change</button>
          </div>
          <p className="hint">A folder auto-names &lt;source&gt;_&lt;timestamp&gt;_merged.&lt;ext&gt; inside it</p>
        </section>

        <section className="section">
          <label className="label">PRESET</label>
          <select className="input select" value={preset} onChange={(e) => applyPreset(e.target.value as Preset)} disabled={isWorking}>
            <option value="custom">Custom</option>
            <option value="lean">LLM review (lean) — gitignore, redact, entry-first, 180k split</option>
            <option value="claude">Claude (cxml, important-last, 180k)</option>
            <option value="archive">Full archive (everything, no split)</option>
            <option value="docs">Docs only (*.md / *.rst / *.txt)</option>
          </select>
        </section>

        <section className="section">
          <label className="label">FORMAT &amp; SIZE</label>
          <div className="grid2">
            <div>
              <span className="field-label">Output format</span>
              <select className="input select" value={format} onChange={(e) => { setFormat(e.target.value); setPreset("custom"); }} disabled={isWorking}>
                <option value="markdown">Markdown</option>
                <option value="cxml">Claude XML (cxml)</option>
                <option value="xml">XML</option>
                <option value="json">JSON</option>
                <option value="plain">Plain text</option>
              </select>
            </div>
            <div>
              <span className="field-label">Ordering</span>
              <select className="input select" value={ordering} onChange={(e) => { setOrdering(e.target.value); setPreset("custom"); }} disabled={isWorking}>
                <option value="path">Alphabetical</option>
                <option value="entry-first">Entry points first</option>
                <option value="important-last">Important last</option>
              </select>
            </div>
          </div>
          <div style={{ marginTop: 10 }}>
            <span className="field-label">Max tokens per file (split if exceeded; blank = no split)</span>
            <input type="number" className="input" value={maxTokens} placeholder="e.g. 180000" min="0"
              onChange={(e) => { setMaxTokens(e.target.value); setPreset("custom"); }} disabled={isWorking} />
          </div>
        </section>

        <section className="section">
          <label className="label">OPTIONS</label>
          {([
            [respectGitignore, setRespectGitignore, "Respect .gitignore / .turbomergerignore"],
            [redactSecrets, setRedactSecrets, "Redact secrets (API keys, tokens, private keys)"],
            [includeTree, setIncludeTree, "Directory tree + table of contents"],
            [contentDetection, setContentDetection, "Include all text files (content-based detection)"],
            [includeHidden, setIncludeHidden, "Include hidden dotfiles (beyond standard config set)"],
            [includeVenv, setIncludeVenv, "Include virtual environments (venv, node_modules)"],
          ] as [boolean, (b: boolean) => void, string][]).map(([val, setter, label], i) => (
            <div className="checkbox-group" key={i}>
              <label className="checkbox-label">
                <input type="checkbox" checked={val} disabled={isWorking}
                  onChange={(e) => { setter(e.target.checked); setPreset("custom"); }} />
                <span>{label}</span>
              </label>
            </div>
          ))}
        </section>

        <section className="section">
          <button className="link-btn" onClick={() => setShowAdvanced(!showAdvanced)}>
            {showAdvanced ? "▾" : "▸"} Advanced filters
          </button>
          {showAdvanced && (
            <div className="advanced">
              <span className="field-label">Include globs (comma-separated; only these if set)</span>
              <input type="text" className="input" value={includeGlobs} placeholder="src/**, *.md"
                onChange={(e) => { setIncludeGlobs(e.target.value); setPreset("custom"); }} disabled={isWorking} />
              <span className="field-label" style={{ marginTop: 8 }}>Exclude globs (comma-separated)</span>
              <input type="text" className="input" value={excludeGlobs} placeholder="**/*.lock, docs/**"
                onChange={(e) => { setExcludeGlobs(e.target.value); setPreset("custom"); }} disabled={isWorking} />
              <div className="checkbox-group" style={{ marginTop: 10 }}>
                <label className="checkbox-label">
                  <input type="checkbox" checked={removeEmptyLines} disabled={isWorking}
                    onChange={(e) => { setRemoveEmptyLines(e.target.checked); setPreset("custom"); }} />
                  <span>Remove empty lines (token saving)</span>
                </label>
              </div>
              <div className="checkbox-group">
                <label className="checkbox-label">
                  <input type="checkbox" checked={truncateBase64} disabled={isWorking}
                    onChange={(e) => { setTruncateBase64(e.target.checked); setPreset("custom"); }} />
                  <span>Truncate long base64/data blobs</span>
                </label>
              </div>
            </div>
          )}
        </section>

        <section className="section">
          {isWorking ? (
            <button className="btn btn-danger btn-full" onClick={handleCancel}>CANCEL</button>
          ) : (
            <button className="btn btn-primary btn-full" onClick={handleMerge} disabled={!sourcePath}>
              {status === "done" ? "MERGE AGAIN" : status === "error" ? "RETRY" : "MERGE"}
            </button>
          )}
        </section>

        {isWorking && (
          <section className="section">
            <div className="progress-container">
              <div className="progress-bar" style={{ width: `${progress?.percentage || 0}%` }} />
            </div>
            <p className="status-text">
              {status === "scanning" ? "Scanning directory..." : progress?.current_file}
            </p>
            {progress && progress.total > 0 && (
              <p className="progress-text">
                {progress.current.toLocaleString()} / {progress.total.toLocaleString()} files ({progress.percentage.toFixed(1)}%)
              </p>
            )}
          </section>
        )}

        {result && status === "done" && (
          <section className="section result-section">
            <div className="stats-grid">
              <div className="stat"><span className="stat-value">{result.files_processed.toLocaleString()}</span><span className="stat-label">merged</span></div>
              <div className="stat"><span className="stat-value">{result.files_skipped.toLocaleString()}</span><span className="stat-label">skipped</span></div>
              <div className="stat"><span className="stat-value">~{formatTokens(result.tokens_o200k)}</span><span className="stat-label">tokens</span></div>
              <div className="stat"><span className="stat-value">{formatDuration(result.duration_ms)}</span><span className="stat-label">time</span></div>
            </div>
            <p className="detection-breakdown">
              {formatBytes(result.total_bytes)} · ~{formatTokens(result.tokens_o200k)} o200k / ~{formatTokens(result.tokens_claude_est)} Claude — {fitHint(result.tokens_claude_est)}
            </p>
            <p className="detection-breakdown">
              {result.files_by_extension.toLocaleString()} by extension
              {result.files_by_content > 0 && `, ${result.files_by_content.toLocaleString()} by content`}
              {result.files_skipped_binary > 0 && `, ${result.files_skipped_binary.toLocaleString()} binary`}
              {result.files_unreadable > 0 && `, ${result.files_unreadable.toLocaleString()} unreadable`}
            </p>
            {result.secrets_redacted > 0 && (
              <p className="detection-breakdown" style={{ color: "var(--warning)" }}>
                ⚠ {result.secrets_redacted.toLocaleString()} secret{result.secrets_redacted === 1 ? "" : "s"} redacted — see Merge Report in the output
              </p>
            )}
            {result.output_paths.length > 1 && (
              <p className="detection-breakdown">Split into {result.output_paths.length} parts</p>
            )}
            <div className="action-buttons">
              <button className="btn btn-success" onClick={() => openThing(result.output_path)}>Open File</button>
              <button className="btn btn-secondary" onClick={() => openFolder(result.output_path)}>Show in Folder</button>
            </div>
            <div className="action-buttons">
              <button className="btn btn-secondary" onClick={() => openThing("https://claude.ai/new")}>claude.ai</button>
              <button className="btn btn-secondary" onClick={() => openThing("https://chatgpt.com")}>chatgpt.com</button>
              <button className="btn btn-secondary" onClick={() => openThing("https://gemini.google.com/app")}>gemini</button>
            </div>
            {result.output_paths.map((p) => <p className="output-path" key={p}>{p}</p>)}
          </section>
        )}

        {error && <section className="section error-section"><p className="error-text">{error}</p></section>}
        {status === "cancelled" && <section className="section cancelled-section"><p className="cancelled-text">Operation cancelled</p></section>}
      </div>

      <footer className="footer"><span>TurboMerger {version ? `v${version}` : ""}</span></footer>
    </div>
  );
}

export default App;
