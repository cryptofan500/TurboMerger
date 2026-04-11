import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import "./styles/global.css";

interface MergeResult {
  output_path: string;
  files_processed: number;
  files_skipped: number;
  total_bytes: number;
  duration_ms: number;
  files_by_extension: number;
  files_by_content: number;
  files_skipped_binary: number;
}

interface ProgressUpdate {
  current: number;
  total: number;
  current_file: string;
  percentage: number;
}

interface MergeOptions {
  folder_path: string;
  output_path: string | null;
  include_venv: boolean;
  include_tree: boolean;
  content_detection: boolean;
}

type Status = "ready" | "scanning" | "merging" | "done" | "error" | "cancelled";

function App() {
  const [status, setStatus] = useState<Status>("ready");
  const [sourcePath, setSourcePath] = useState<string>("");
  const [outputPath, setOutputPath] = useState<string>("");
  const [includeVenv, setIncludeVenv] = useState<boolean>(false);
  const [includeTree, setIncludeTree] = useState<boolean>(true);
  const [contentDetection, setContentDetection] = useState<boolean>(true);
  const [result, setResult] = useState<MergeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<ProgressUpdate | null>(null);

  // Initialize default output path
  useEffect(() => {
    invoke<string>("get_downloads_path").then(setOutputPath).catch(console.error);
  }, []);

  // Listen for progress updates
  useEffect(() => {
    const unlisten = listen<ProgressUpdate>("merge-progress", (event) => {
      setProgress(event.payload);
      if (event.payload.total > 0) {
        setStatus("merging");
      }
    });
    return () => { unlisten.then(f => f()); };
  }, []);

  const selectSource = async () => {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "Select Codebase to Merge",
    });
    if (selected && typeof selected === "string") {
      setSourcePath(selected);
      // Auto-generate output name based on source
      const folderName = selected.split(/[/\\]/).pop() || "merged";
      const timestamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
      const defaultOutput = await invoke<string>("get_downloads_path");
      setOutputPath(`${defaultOutput}\\${folderName}_${timestamp}_merged.md`);
    }
  };

  const selectOutput = async () => {
    const selected = await save({
      defaultPath: outputPath,
      filters: [{ name: "Markdown", extensions: ["md"] }],
      title: "Save Merged Output As",
    });
    if (selected) {
      setOutputPath(selected);
    }
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

      const options: MergeOptions = {
        folder_path: sourcePath,
        output_path: outputPath || null,
        include_venv: includeVenv,
        include_tree: includeTree,
        content_detection: contentDetection,
      };

      const mergeResult = await invoke<MergeResult>("merge_folder", { options });

      setResult(mergeResult);
      setStatus("done");
    } catch (err) {
      const errorMsg = String(err);
      if (errorMsg.includes("cancelled")) {
        setStatus("cancelled");
      } else {
        setError(errorMsg);
        setStatus("error");
      }
    }
  };

  const handleOpenFile = async () => {
    if (result?.output_path) {
      await invoke("open_file", { path: result.output_path });
    }
  };

  const handleOpenFolder = async () => {
    if (result?.output_path) {
      await invoke("open_folder", { path: result.output_path });
    }
  };

  const formatBytes = (bytes: number): string => {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  };

  const formatDuration = (ms: number): string => {
    if (ms < 1000) return `${ms}ms`;
    if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
    return `${(ms / 60000).toFixed(1)}m`;
  };

  const isWorking = status === "scanning" || status === "merging";

  return (
    <div className="app">
      <div className="container">
        <header className="header">
          <h1 className="title">TurboMerger</h1>
          <span className="version">v7.1</span>
        </header>

        {/* Source Selection */}
        <section className="section">
          <label className="label">SOURCE CODEBASE</label>
          <div className="input-row">
            <input
              type="text"
              className="input"
              value={sourcePath}
              readOnly
              placeholder="Select a folder to merge..."
            />
            <button className="btn btn-secondary" onClick={selectSource} disabled={isWorking}>
              Browse
            </button>
          </div>
        </section>

        {/* Output Selection */}
        <section className="section">
          <label className="label">OUTPUT DESTINATION</label>
          <div className="input-row">
            <input
              type="text"
              className="input"
              value={outputPath}
              readOnly
              placeholder="Output path..."
            />
            <button className="btn btn-secondary" onClick={selectOutput} disabled={isWorking}>
              Change
            </button>
          </div>
        </section>

        {/* Configuration */}
        <section className="section">
          <label className="label">OPTIONS</label>
          <div className="checkbox-group">
            <label className="checkbox-label">
              <input
                type="checkbox"
                checked={includeVenv}
                onChange={(e) => setIncludeVenv(e.target.checked)}
                disabled={isWorking}
              />
              <span>Include virtual environments (venv, node_modules)</span>
            </label>
            <p className="checkbox-hint">Warning: Can slow down scanning significantly</p>
          </div>
          <div className="checkbox-group">
            <label className="checkbox-label">
              <input
                type="checkbox"
                checked={includeTree}
                onChange={(e) => setIncludeTree(e.target.checked)}
                disabled={isWorking}
              />
              <span>Generate directory tree</span>
            </label>
          </div>
          <div className="checkbox-group">
            <label className="checkbox-label">
              <input
                type="checkbox"
                checked={contentDetection}
                onChange={(e) => setContentDetection(e.target.checked)}
                disabled={isWorking}
              />
              <span>Include all text files (content-based detection)</span>
            </label>
            <p className="checkbox-hint" style={{ color: 'var(--text-muted)' }}>
              Detects text files by reading content, not just file extension
            </p>
          </div>
        </section>

        {/* Action Buttons */}
        <section className="section">
          {isWorking ? (
            <button className="btn btn-danger btn-full" onClick={handleCancel}>
              CANCEL
            </button>
          ) : (
            <button
              className="btn btn-primary btn-full"
              onClick={handleMerge}
              disabled={!sourcePath}
            >
              {status === "ready" && "MERGE"}
              {status === "done" && "MERGE AGAIN"}
              {status === "error" && "RETRY"}
              {status === "cancelled" && "MERGE"}
            </button>
          )}
        </section>

        {/* Progress Bar */}
        {isWorking && (
          <section className="section">
            <div className="progress-container">
              <div
                className="progress-bar"
                style={{ width: `${progress?.percentage || 0}%` }}
              />
            </div>
            <p className="status-text">
              {status === "scanning" && "Scanning directory..."}
              {status === "merging" && progress && (
                <>{progress.current_file}</>
              )}
            </p>
            {progress && progress.total > 0 && (
              <p className="progress-text">
                {progress.current.toLocaleString()} / {progress.total.toLocaleString()} files
                ({progress.percentage.toFixed(1)}%)
              </p>
            )}
          </section>
        )}

        {/* Results */}
        {result && status === "done" && (
          <section className="section result-section">
            <div className="stats-grid">
              <div className="stat">
                <span className="stat-value">{result.files_processed.toLocaleString()}</span>
                <span className="stat-label">merged</span>
              </div>
              <div className="stat">
                <span className="stat-value">{result.files_skipped.toLocaleString()}</span>
                <span className="stat-label">skipped</span>
              </div>
              <div className="stat">
                <span className="stat-value">{formatBytes(result.total_bytes)}</span>
                <span className="stat-label">size</span>
              </div>
              <div className="stat">
                <span className="stat-value">{formatDuration(result.duration_ms)}</span>
                <span className="stat-label">time</span>
              </div>
            </div>
            {(result.files_by_extension > 0 || result.files_by_content > 0) && (
              <p className="detection-breakdown">
                {result.files_by_extension.toLocaleString()} by extension
                {result.files_by_content > 0 && `, ${result.files_by_content.toLocaleString()} by content detection`}
                {result.files_skipped_binary > 0 && `, ${result.files_skipped_binary.toLocaleString()} binary skipped`}
              </p>
            )}
            <div className="action-buttons">
              <button className="btn btn-success" onClick={handleOpenFile}>
                Open File
              </button>
              <button className="btn btn-secondary" onClick={handleOpenFolder}>
                Open Folder
              </button>
            </div>
            <p className="output-path">{result.output_path}</p>
          </section>
        )}

        {/* Error Display */}
        {error && (
          <section className="section error-section">
            <p className="error-text">{error}</p>
          </section>
        )}

        {/* Cancelled Display */}
        {status === "cancelled" && (
          <section className="section cancelled-section">
            <p className="cancelled-text">Operation cancelled</p>
          </section>
        )}
      </div>

      <footer className="footer">
        <span>TurboMerger v7.1.0</span>
      </footer>
    </div>
  );
}

export default App;
