// Apply-back panel (T3-3): paste an LLM reply, review per-file side-by-side
// diffs, apply the accepted subset (with automatic backups), restore if needed.
// Parsing/diffing/writing all happen in Rust; this panel never sees file
// contents beyond the diff lines it renders.

import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface DiffLine {
  tag: "eq" | "del" | "ins";
  old: number | null;
  new: number | null;
  text: string;
}

interface PreviewFile {
  rel_path: string;
  action: string; // "modify" | "create" | "delete"
  ok: boolean;
  note: string;
  identical: boolean;
  adds: number;
  dels: number;
  diff: DiffLine[];
  diff_truncated: boolean;
}

interface Preview {
  root: string;
  files: PreviewFile[];
}

interface ApplyFailure {
  rel_path: string;
  reason: string;
}

interface ApplyOutcome {
  backup_dir: string | null;
  applied: string[];
  failed: ApplyFailure[];
}

interface RestoreOutcome {
  backup_dir: string;
  restored: string[];
  deleted: string[];
}

/** Pair delete/insert runs into side-by-side rows. */
interface Row {
  left?: DiffLine;
  right?: DiffLine;
}

function pairRows(diff: DiffLine[]): Row[] {
  const rows: Row[] = [];
  let i = 0;
  while (i < diff.length) {
    if (diff[i].tag === "eq") {
      rows.push({ left: diff[i], right: diff[i] });
      i++;
      continue;
    }
    const dels: DiffLine[] = [];
    const inss: DiffLine[] = [];
    while (i < diff.length && diff[i].tag === "del") dels.push(diff[i++]);
    while (i < diff.length && diff[i].tag === "ins") inss.push(diff[i++]);
    const n = Math.max(dels.length, inss.length);
    for (let k = 0; k < n; k++) rows.push({ left: dels[k], right: inss[k] });
  }
  return rows;
}

function FileDiff({ file }: { file: PreviewFile }) {
  const rows = useMemo(() => pairRows(file.diff), [file.diff]);
  return (
    <div className="diff-scroll">
      <div className="diff-grid">
        {rows.map((r, idx) => (
          <div className="diff-row" key={idx}>
            <span className="diff-lineno">{r.left?.old ?? ""}</span>
            <span className={`diff-cell ${r.left ? (r.left.tag === "del" ? "diff-del" : "diff-eq") : "diff-void"}`}>
              {r.left?.text ?? ""}
            </span>
            <span className="diff-lineno">{r.right?.new ?? ""}</span>
            <span className={`diff-cell ${r.right ? (r.right.tag === "ins" ? "diff-ins" : "diff-eq") : "diff-void"}`}>
              {r.right?.text ?? ""}
            </span>
          </div>
        ))}
      </div>
      {file.diff_truncated && (
        <p className="hint">Diff preview truncated — the full change still applies.</p>
      )}
    </div>
  );
}

export default function ApplyPanel({ root, disabled }: { root: string; disabled: boolean }) {
  const [reply, setReply] = useState("");
  const [preview, setPreview] = useState<Preview | null>(null);
  const [accepted, setAccepted] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [outcome, setOutcome] = useState<ApplyOutcome | null>(null);
  const [restored, setRestored] = useState<RestoreOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const appliable = preview ? preview.files.filter((f) => f.ok && !f.identical) : [];

  const handleParse = async () => {
    setBusy(true);
    setError(null);
    setOutcome(null);
    setRestored(null);
    try {
      const p = await invoke<Preview>("preview_apply", { root, reply });
      setPreview(p);
      setAccepted(new Set(p.files.filter((f) => f.ok && !f.identical).map((f) => f.rel_path)));
      setExpanded(new Set());
    } catch (err) {
      setPreview(null);
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleApply = async () => {
    setBusy(true);
    setError(null);
    try {
      const o = await invoke<ApplyOutcome>("apply_accepted", {
        root,
        accept: [...accepted],
      });
      setOutcome(o);
      // The preview is consumed server-side; a new round needs a re-parse.
      setPreview(null);
      setAccepted(new Set());
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleRestore = async () => {
    setBusy(true);
    setError(null);
    try {
      setRestored(await invoke<RestoreOutcome>("restore_backup", { root }));
      setOutcome(null);
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  const toggle = (set: Set<string>, key: string, update: (s: Set<string>) => void) => {
    const next = new Set(set);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    update(next);
  };

  const badge = (f: PreviewFile) =>
    !f.ok ? "badge badge-blocked" : f.action === "create" ? "badge badge-create" : "badge badge-modify";

  return (
    <div className="apply-panel">
      <p className="hint">
        Paste the LLM's reply (fenced blocks under <code>## path</code> headers, cxml documents,
        or unified diffs). Nothing is written until you review and click Apply; originals are
        backed up to <code>.turbomerger/backups/</code> in the source folder.
      </p>
      <textarea
        className="input apply-textarea"
        value={reply}
        onChange={(e) => setReply(e.target.value)}
        placeholder={"## src/main.rs\n```rust\nfn main() { ... }\n```\n\n--- a/lib.py\n+++ b/lib.py\n@@ -1,3 +1,3 @@ ..."}
        disabled={disabled || busy}
      />
      <div className="apply-actions">
        <button
          className="btn btn-primary"
          onClick={handleParse}
          disabled={disabled || busy || !reply.trim()}
        >
          Preview changes
        </button>
        <button className="btn btn-secondary" onClick={handleRestore} disabled={disabled || busy}
          title="Undo the most recent apply for this folder from its backup">
          Restore last apply
        </button>
      </div>

      {preview && (
        <div className="apply-list">
          {preview.files.map((f) => (
            <div className="apply-card" key={f.rel_path}>
              <div className="apply-card-head">
                <label className="checkbox-label apply-card-title">
                  <input
                    type="checkbox"
                    checked={accepted.has(f.rel_path)}
                    disabled={!f.ok || f.identical || busy}
                    onChange={() => toggle(accepted, f.rel_path, setAccepted)}
                  />
                  <span className={badge(f)}>{f.ok ? f.action : f.action === "delete" ? "delete" : "blocked"}</span>
                  <span className="apply-path">{f.rel_path}</span>
                </label>
                <span className="apply-stats">
                  {f.ok && !f.identical && (
                    <>
                      <span className="diffstat-add">+{f.adds}</span>{" "}
                      <span className="diffstat-del">−{f.dels}</span>
                    </>
                  )}
                  {f.identical && <span className="hint">no changes</span>}
                  {f.ok && !f.identical && f.diff.length > 0 && (
                    <button
                      className="link-btn"
                      onClick={() => toggle(expanded, f.rel_path, setExpanded)}
                    >
                      {expanded.has(f.rel_path) ? "hide diff" : "show diff"}
                    </button>
                  )}
                </span>
              </div>
              {!f.ok && <p className="apply-note">{f.note}</p>}
              {expanded.has(f.rel_path) && f.ok && !f.identical && <FileDiff file={f} />}
            </div>
          ))}
          <div className="apply-actions">
            <button
              className="btn btn-success"
              onClick={handleApply}
              disabled={busy || accepted.size === 0}
            >
              Apply {accepted.size} of {appliable.length} file{appliable.length === 1 ? "" : "s"}
            </button>
          </div>
        </div>
      )}

      {outcome && (
        <div className="apply-result">
          <p className="apply-ok">
            ✓ Applied {outcome.applied.length} file{outcome.applied.length === 1 ? "" : "s"}
          </p>
          {outcome.failed.map((f) => (
            <p className="apply-note" key={f.rel_path}>
              ✗ {f.rel_path} — {f.reason}
            </p>
          ))}
          {outcome.backup_dir && (
            <p className="hint">
              Originals backed up to <span className="output-path-inline">{outcome.backup_dir}</span>
              {" · "}
              <button className="link-btn" onClick={handleRestore} disabled={busy}>
                Restore
              </button>
            </p>
          )}
        </div>
      )}

      {restored && (
        <div className="apply-result">
          <p className="apply-ok">
            ✓ Restored {restored.restored.length} file{restored.restored.length === 1 ? "" : "s"}
            {restored.deleted.length > 0 && `, removed ${restored.deleted.length} created`}
          </p>
          <p className="hint">
            From <span className="output-path-inline">{restored.backup_dir}</span>
          </p>
        </div>
      )}

      {error && <p className="error-text" style={{ marginTop: 8 }}>{error}</p>}
    </div>
  );
}
