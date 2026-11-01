import { useState, useEffect, useRef } from "react";
import { invoke, Channel } from "@tauri-apps/api/core";
import { open, save, ask } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { homeDir } from "@tauri-apps/api/path";
import CuratePanel, { ScanReport } from "./components/CuratePanel";
import ApplyPanel from "./components/ApplyPanel";
import "./styles/global.css";

interface MergeResult {
  output_path: string;
  output_paths: string[];
  files_processed: number;
  files_skipped: number;
  /** Inputs whose content is NOT in the output (documents, photos, too large…). */
  not_captured: number;
  total_bytes: number;
  duration_ms: number;
  files_by_extension: number;
  files_by_content: number;
  files_skipped_binary: number;
  files_unreadable: number;
  secrets_redacted: number;
  tokens_o200k: number;
  tokens_claude_est: number;
  skill_path: string | null;
  /** Size of everything written (all parts). */
  output_bytes?: number;
}

/** What MERGE would write, when it is too big for one file (S8 guard). */
interface BigMerge {
  tokens: number;
  bytes: number;
  files: number;
}

/** A selection plan (`.tmsel.toml`) loaded for the next merge. */
interface LoadedPlan {
  path: string;
  // The parsed plan, sent back unchanged with the merge options.
  plan: { complete?: boolean; file?: { path: string }[] };
}

// One file above either limit needs a click first: Geany froze opening a
// 561 MB merge (S7 test, 5,758 files written as one 179 M-token file).
const BIG_MERGE_TOKENS = 1_500_000;
const BIG_MERGE_BYTES = 50 * 1048576;
const BIG_OUTPUT_BYTES = 30 * 1048576;
const SPLIT_TOKENS = "180000";
// The curate tree builds every row in this window; past this, use the CLI.
const GUI_CURATE_MAX_FILES = 100_000;

/** One progress report from a running job (tm_core::progress snapshot). */
interface JobProgress {
  stage: "scan" | "classify" | "merge" | "ocr";
  done: number;
  /** 0 while unknown. */
  total: number;
  current: string;
  elapsed_ms: number;
  /** Shown only after the job has run for a minute. */
  eta_ms: number | null;
  eta_band_ms: number | null;
  /** Set when nothing has moved for a while. */
  stalled_ms: number | null;
  /** True on one update only: the run looks longer than 10 minutes. */
  ask_long_run: boolean;
}

/** The open "proceed or cancel?" question. It shows the live estimate (one
 * number on screen, not a stale one beside a fresh one); `elapsedMs` is when
 * it was asked, for the "still counting files" case. */
interface LongRunQuestion {
  elapsedMs: number;
}

/** Local clock time `ms` from now: "14:05" today, "Thu 01:00" on another day. */
const finishClock = (ms: number) => {
  const now = new Date();
  const at = new Date(now.getTime() + ms);
  const time = at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return at.toDateString() === now.toDateString()
    ? time
    : `${at.toLocaleDateString([], { weekday: "short" })} ${time}`;
};

type Status = "ready" | "scanning" | "merging" | "cancelling" | "done" | "error" | "cancelled";

const newJobId = () =>
  typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID()
    : `job-${Date.now()}-${Math.random().toString(36).slice(2)}`;
type Preset = "custom" | "lean" | "archive" | "docs" | "claude";

const SETTINGS_KEY = "turbomerger.settings.v2";
/** v1 saved redaction and .gitignore on; v2 starts both off (every text file
 *  is taken). v1 is carried over once, without those two. */
const LEGACY_SETTINGS_KEY = "turbomerger.settings.v1";
const RESET_FROM_V1: ReadonlySet<string> = new Set(["redactSecrets", "respectGitignore"]);

/** Settings saved by an earlier session; anything missing or malformed is ignored. */
interface SavedSettings {
  includeVenv?: boolean; includeTree?: boolean; contentDetection?: boolean;
  respectGitignore?: boolean; includeHidden?: boolean; redactSecrets?: boolean;
  format?: string; ordering?: string; maxTokens?: string; includeGlobs?: string;
  excludeGlobs?: string; removeEmptyLines?: boolean; truncateBase64?: boolean;
  compress?: boolean; stripComments?: boolean; gitDiff?: boolean; gitLogCount?: string;
  emitSkill?: boolean; sourcePath?: string; extractImagesText?: boolean;
  ocrAllPictures?: boolean; keepFigures?: boolean;
}

function loadSettings(): SavedSettings {
  try {
    let raw = localStorage.getItem(SETTINGS_KEY);
    const fromV1 = raw === null;
    if (fromV1) raw = localStorage.getItem(LEGACY_SETTINGS_KEY);
    const s: unknown = raw ? JSON.parse(raw) : null;
    if (!s || typeof s !== "object") return {};
    const out: Record<string, unknown> = {};
    const kinds: Record<keyof SavedSettings, "boolean" | "string"> = {
      includeVenv: "boolean", includeTree: "boolean", contentDetection: "boolean",
      respectGitignore: "boolean", includeHidden: "boolean", redactSecrets: "boolean",
      format: "string", ordering: "string", maxTokens: "string", includeGlobs: "string",
      excludeGlobs: "string", removeEmptyLines: "boolean", truncateBase64: "boolean",
      compress: "boolean", stripComments: "boolean", gitDiff: "boolean",
      gitLogCount: "string", emitSkill: "boolean", sourcePath: "string",
      extractImagesText: "boolean", ocrAllPictures: "boolean", keepFigures: "boolean",
    };
    for (const [key, kind] of Object.entries(kinds)) {
      if (fromV1 && RESET_FROM_V1.has(key)) continue;
      const v = (s as Record<string, unknown>)[key];
      if (typeof v === kind) out[key] = v;
    }
    return out as SavedSettings;
  } catch {
    return {}; // corrupt settings
  }
}

function App() {
  // Read once, before the first render (not setState inside an effect).
  const [saved] = useState(loadSettings);
  const [status, setStatus] = useState<Status>("ready");
  const [version, setVersion] = useState<string>("");
  const [sourcePath, setSourcePath] = useState<string>(saved.sourcePath ?? "");
  const [outputPath, setOutputPath] = useState<string>("");

  const [includeVenv, setIncludeVenv] = useState(saved.includeVenv ?? false);
  const [includeTree, setIncludeTree] = useState(saved.includeTree ?? true);
  const [contentDetection, setContentDetection] = useState(saved.contentDetection ?? true);
  const [respectGitignore, setRespectGitignore] = useState(saved.respectGitignore ?? false);
  const [includeHidden, setIncludeHidden] = useState(saved.includeHidden ?? false);
  const [redactSecrets, setRedactSecrets] = useState(saved.redactSecrets ?? false);
  const [extractImagesText, setExtractImagesText] = useState(saved.extractImagesText ?? false);
  const [ocrAllPictures, setOcrAllPictures] = useState(saved.ocrAllPictures ?? false);
  const [keepFigures, setKeepFigures] = useState(saved.keepFigures ?? false);
  const [format, setFormat] = useState<string>(saved.format ?? "markdown");
  const [ordering, setOrdering] = useState<string>(saved.ordering ?? "path");
  const [maxTokens, setMaxTokens] = useState<string>(saved.maxTokens ?? "");
  const [includeGlobs, setIncludeGlobs] = useState<string>(saved.includeGlobs ?? "");
  const [excludeGlobs, setExcludeGlobs] = useState<string>(saved.excludeGlobs ?? "");
  const [removeEmptyLines, setRemoveEmptyLines] = useState(saved.removeEmptyLines ?? false);
  const [truncateBase64, setTruncateBase64] = useState(saved.truncateBase64 ?? false);
  const [compress, setCompress] = useState(saved.compress ?? false);
  const [stripComments, setStripComments] = useState(saved.stripComments ?? false);
  const [gitDiff, setGitDiff] = useState(saved.gitDiff ?? false);
  const [gitLogCount, setGitLogCount] = useState<string>(saved.gitLogCount ?? "");
  const [emitSkill, setEmitSkill] = useState(saved.emitSkill ?? false);
  // CLI parity (S14): the merge options the CLI has and the app lacked. Per
  // run, not saved; refused combinations come back from tm-core's check.
  const [resumeMode, setResumeMode] = useState<"off" | "checkpoint" | "resume">("off");
  const [completeTwin, setCompleteTwin] = useState(false);
  const [embedBinaryMb, setEmbedBinaryMb] = useState<string>("");
  const [budgetTokens, setBudgetTokens] = useState<string>("");
  const [gridChunkRows, setGridChunkRows] = useState<string>("");
  const [reproducible, setReproducible] = useState(false);
  const [showSourcePath, setShowSourcePath] = useState(false);
  const [budgetNote, setBudgetNote] = useState<string | null>(null);
  const [preset, setPreset] = useState<Preset>("custom");
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [showApply, setShowApply] = useState(false);

  // Remote packing (T2-8): PAT stays in memory only — never persisted.
  const [pat, setPat] = useState<string>("");

  // Curate state (T2-1/T2-2/T2-7): exclusions + rescues for the CURRENT source.
  const [scanReport, setScanReport] = useState<ScanReport | null>(null);
  const [autoScanNote, setAutoScanNote] = useState<string | null>(null);
  // The drop listener is registered once; it reaches the current autoScan through this ref.
  const autoScanRef = useRef<(path: string) => void>(() => {});
  const [curateOpen, setCurateOpen] = useState(false);
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
  const [forceInclude, setForceInclude] = useState<Set<string>>(new Set());

  // Watch mode (T2-6)
  const [watching, setWatching] = useState(false);
  const [watchOutput, setWatchOutput] = useState<string>("");

  const [result, setResult] = useState<MergeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<JobProgress | null>(null);
  const [longRun, setLongRun] = useState<LongRunQuestion | null>(null);
  const [bigMerge, setBigMerge] = useState<BigMerge | null>(null);
  const [plan, setPlan] = useState<LoadedPlan | null>(null);
  const [dropOverMb, setDropOverMb] = useState<string>("25");
  // The running merge/scan, so Cancel reaches exactly that job.
  const [jobId, setJobId] = useState<string | null>(null);

  const isRemote =
    /^(https?:\/\/|git@)/.test(sourcePath.trim()) ||
    /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(sourcePath.trim());

  // Output folder + version on mount (async results, not synchronous setState).
  useEffect(() => {
    invoke<string>("get_downloads_path").then(setOutputPath).catch(console.error);
    getVersion().then(setVersion).catch(console.error);
  }, []);

  // Persist settings on change (never the PAT).
  useEffect(() => {
    const s = {
      includeVenv, includeTree, contentDetection, respectGitignore, includeHidden,
      redactSecrets, format, ordering, maxTokens, includeGlobs, excludeGlobs,
      removeEmptyLines, truncateBase64, compress, stripComments, gitDiff,
      gitLogCount, emitSkill, sourcePath, extractImagesText, ocrAllPictures, keepFigures,
    };
    try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(s)); } catch { /* quota */ }
  }, [includeVenv, includeTree, contentDetection, respectGitignore, includeHidden,
      redactSecrets, format, ordering, maxTokens, includeGlobs, excludeGlobs,
      removeEmptyLines, truncateBase64, compress, stripComments, gitDiff,
      gitLogCount, emitSkill, sourcePath, extractImagesText, ocrAllPictures, keepFigures]);

  // Selection is per-project: a new source resets it (restored on scan).
  const changeSource = (path: string) => {
    setSourcePath(path);
    setScanReport(null);
    setCurateOpen(false);
    setExcluded(new Set());
    setForceInclude(new Set());
    setWatching(false);
  };

  // Persist the curation (exclusions + rescues) per project.
  const selectionKey = (src: string) => `turbomerger.selection.${src}`;
  useEffect(() => {
    if (!scanReport) return;
    try {
      localStorage.setItem(
        selectionKey(scanReport.root),
        JSON.stringify({ excluded: [...excluded], forceInclude: [...forceInclude] })
      );
    } catch { /* quota */ }
  }, [excluded, forceInclude, scanReport]);

  // Watch-mode results arrive as events; job progress comes over channels.
  useEffect(() => {
    const unlistenWatch = listen<MergeResult>("watch-merged", (event) => {
      setResult(event.payload);
      setStatus("done");
    });
    const unlistenWatchErr = listen<string>("watch-error", (event) => {
      setError(event.payload);
      setStatus("error");
    });
    return () => {
      unlistenWatch.then((f) => f());
      unlistenWatchErr.then((f) => f());
    };
  }, []);

  /** A channel that shows a job's progress; merges switch the status once files flow. */
  const progressChannel = (merging: boolean) => {
    setLongRun(null); // a new job: no question left over from the last one
    const ch = new Channel<JobProgress>();
    ch.onmessage = (p) => {
      setProgress(p);
      if (p.ask_long_run) {
        setLongRun({ elapsedMs: p.elapsed_ms });
      }
      if (merging && p.stage === "merge") {
        setStatus((s) => (s === "scanning" ? "merging" : s));
      }
    };
    return ch;
  };

  // Drag-and-drop a folder onto the window.
  useEffect(() => {
    const p = getCurrentWebview().onDragDropEvent((event) => {
      if (
        !watching &&
        event.payload.type === "drop" &&
        event.payload.paths.length > 0
      ) {
        const dropPath = event.payload.paths[0];
        changeSource(dropPath);
        autoScanRef.current(dropPath);
      }
    });
    return () => { p.then((f) => f()); };
  }, [watching]);

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
    if (selected && typeof selected === "string") {
      changeSource(selected);
      autoScan(selected);
    }
  };

  const selectOutput = async () => {
    const selected = await save({
      defaultPath: outputPath,
      filters: [{ name: "Output", extensions: ["md", "xml", "json", "txt"] }],
      title: "Save Merged Output As",
    });
    if (selected) setOutputPath(selected);
  };

  // Ask the running job to stop; its promise settling is the acknowledgement.
  const handleCancel = async () => {
    if (!jobId) return;
    setStatus("cancelling");
    await invoke<boolean>("cancel_job", { job: jobId }).catch(console.error);
  };

  // A positive whole number, or null (blank / 0 = off, as on the CLI).
  const positive = (s: string) => {
    const n = parseInt(s, 10);
    return Number.isFinite(n) && n > 0 ? n : null;
  };
  const buildOptions = (src?: string, maxOverride?: string) => {
    const toList = (s: string) => s.split(/[,\n]/).map((x) => x.trim()).filter(Boolean);
    // Exact include list only when the user actually excluded something in
    // the curate panel for THIS source.
    const selected =
      !plan && scanReport && excluded.size > 0
        ? scanReport.included.map((e) => e.path).filter((p) => !excluded.has(p))
        : null;
    const maxText = maxOverride ?? maxTokens;
    return {
      folder_path: src ?? sourcePath,
      output_path: outputPath || null,
      include_venv: includeVenv,
      include_tree: includeTree,
      content_detection: contentDetection,
      respect_gitignore: respectGitignore,
      include_hidden: includeHidden,
      redact_secrets: redactSecrets,
      extract_images_text: extractImagesText,
      ocr_all_pictures: extractImagesText && ocrAllPictures,
      keep_figures: extractImagesText && keepFigures,
      format,
      ordering,
      max_tokens: maxText ? parseInt(maxText, 10) || 0 : 0,
      include_globs: toList(includeGlobs),
      exclude_globs: toList(excludeGlobs),
      remove_empty_lines: removeEmptyLines,
      truncate_base64: truncateBase64,
      compress,
      strip_comments: stripComments,
      git_diff: gitDiff,
      git_log_count: gitLogCount ? parseInt(gitLogCount, 10) || 0 : 0,
      emit_skill: emitSkill,
      selected_paths: selected,
      force_include: [...forceInclude],
      selection: plan ? plan.plan : null,
      complete: (plan?.plan.complete ?? false) || completeTwin,
      // Blank = no cap (CLI default); 0 is a real cap.
      embed_binary: embedBinaryMb.trim() === "" ? null : Math.max(0, parseInt(embedBinaryMb, 10) || 0),
      budget_tokens: positive(budgetTokens),
      grid_chunk_rows: positive(gridChunkRows),
      reproducible,
      show_source_path: showSourcePath,
      checkpoint: resumeMode === "checkpoint",
      resume: resumeMode === "resume",
    };
  };

  // What MERGE would write as ONE file: the curated scan when there is one,
  // else a cheap count of the folder (bytes / 4 ≈ tokens).
  const mergeEstimate = async (): Promise<BigMerge | null> => {
    if (scanReport) {
      const kept = scanReport.included.filter((e) => !excluded.has(e.path));
      return {
        tokens: kept.reduce((n, e) => n + e.tokens, 0),
        bytes: kept.reduce((n, e) => n + e.size, 0),
        files: kept.length,
      };
    }
    try {
      const c = await invoke<{ files: number; bytes: number }>("count_folder", { path: sourcePath });
      return { tokens: Math.round(c.bytes / 4), bytes: c.bytes, files: c.files };
    } catch {
      return null;
    }
  };

  const handleMerge = async (force = false, maxOverride?: string) => {
    if (!sourcePath) return;
    const maxText = maxOverride ?? maxTokens;
    if (!force && !isRemote && !plan && !(maxText && parseInt(maxText, 10) > 0)) {
      const est = await mergeEstimate();
      if (est && (est.tokens > BIG_MERGE_TOKENS || est.bytes > BIG_MERGE_BYTES)) {
        setBigMerge(est);
        return;
      }
    }
    setBigMerge(null);
    const job = newJobId();
    try {
      setJobId(job);
      setStatus("scanning");
      setError(null);
      setResult(null);
      setProgress(null);

      const options = buildOptions(undefined, maxOverride);
      const progress = progressChannel(true);
      const mergeResult = isRemote
        ? await invoke<MergeResult>("pack_remote", {
            url: sourcePath.trim(),
            pat: pat || null,
            options,
            job,
            progress,
          })
        : await invoke<MergeResult>("merge_folder", { options, job, progress });
      setResult(mergeResult);
      const budget = positive(budgetTokens);
      setBudgetNote(
        budget !== null && mergeResult.tokens_o200k > budget
          ? `Over the token budget: ~${mergeResult.tokens_o200k.toLocaleString()} tokens > ${budget.toLocaleString()} (the CLI would exit 5). The output was written.`
          : null
      );
      setStatus("done");
    } catch (err) {
      const msg = String(err);
      if (msg.includes("cancelled")) setStatus("cancelled");
      else { setError(msg); setStatus("error"); }
    } finally {
      setJobId(null);
      setProgress(null);
    }
  };

  // A folder is scanned automatically only when a cheap count (no file is
  // read) shows it is small; drives, the home folder and big trees wait for
  // "Scan & curate", so choosing a folder can never freeze the window.
  const AUTO_SCAN_MAX_FILES = 5000;
  const AUTO_SCAN_MAX_BYTES = 500 * 1024 * 1024;
  const autoScan = async (path: string) => {
    setAutoScanNote(null);
    setScanReport(null);
    const trim = (p: string) => p.replace(/[\\/]+$/, "");
    const home = await homeDir().catch(() => "");
    if (/^([A-Za-z]:)?$/.test(trim(path)) || (home && trim(path) === trim(home))) {
      setAutoScanNote("A drive or your home folder: use Scan & curate to choose the files.");
      return;
    }
    try {
      const c = await invoke<{ files: number; bytes: number; truncated: boolean }>("count_folder", { path });
      if (c.truncated || c.files > AUTO_SCAN_MAX_FILES || c.bytes > AUTO_SCAN_MAX_BYTES) {
        setAutoScanNote(
          `Large folder (${c.truncated ? "more than " : ""}${c.files.toLocaleString()} files, ${(c.bytes / 1048576).toFixed(0)} MB): use Scan & curate to choose the files.`
        );
        return;
      }
    } catch {
      return;
    }
    handleScan(path, false);
  };

  useEffect(() => {
    autoScanRef.current = (path: string) => void autoScan(path);
  });

  const handleScan = async (overridePath?: string, openModal = true) => {
    const target = overridePath ?? sourcePath;
    if (!target || isRemote) return;
    const job = newJobId();
    try {
      setJobId(job);
      setStatus("scanning");
      setError(null);
      setProgress(null);
      const report = await invoke<ScanReport>("scan_folder", {
        options: buildOptions(target),
        job,
        progress: progressChannel(false),
      });
      setScanReport(report);
      // Restore this project's saved curation, dropping stale paths.
      try {
        const raw = localStorage.getItem(selectionKey(report.root));
        if (raw) {
          const s = JSON.parse(raw);
          const valid = new Set(report.included.map((e) => e.path));
          setExcluded(new Set((s.excluded || []).filter((p: string) => valid.has(p))));
          setForceInclude(new Set(s.forceInclude || []));
        } else {
          setExcluded(new Set());
          setForceInclude(new Set());
        }
      } catch { /* corrupt selection */ }
      if (openModal) setCurateOpen(true);
      setStatus("ready");
    } catch (err) {
      const msg = String(err);
      if (msg.includes("cancelled")) setStatus("cancelled");
      else { setError(msg); setStatus("error"); }
    } finally {
      setJobId(null);
      setProgress(null);
    }
  };

  const applyDropDumps = () => {
    if (!scanReport) return;
    const next = new Set(excluded);
    for (const e of scanReport.included) {
      const p = e.path.toLowerCase();
      if (
        (p.endsWith(".json") && e.size > 500 * 1024) ||
        p.endsWith(".sqlite") ||
        p.endsWith(".db") ||
        p.includes("out/fast/") ||
        p.includes("cache/")
      ) {
        next.add(e.path);
      }
    }
    setExcluded(next);
  };

  const applyDropMedia = () => {
    if (!scanReport) return;
    const next = new Set(excluded);
    for (const e of scanReport.included) {
      const p = e.path.toLowerCase();
      if (
        p.includes("podcast") ||
        p.includes("transcript") ||
        p.includes("audio") ||
        p.endsWith(".mp3") ||
        p.endsWith(".wav") ||
        p.endsWith(".m4a")
      ) {
        next.add(e.path);
      }
    }
    setExcluded(next);
  };

  const applyDocsAndTables = () => {
    if (!scanReport) return;
    const next = new Set(excluded);
    for (const e of scanReport.included) {
      const p = e.path.toLowerCase();
      // Every workbook format the engine reads as TM-GRID (S13: .xlsb,
      // .xlsm and .ods were dropped by this chip).
      const isDocOrTable =
        p.endsWith(".xlsx") ||
        p.endsWith(".xlsm") ||
        p.endsWith(".xlsb") ||
        p.endsWith(".xls") ||
        p.endsWith(".ods") ||
        p.endsWith(".md") ||
        p.endsWith(".markdown") ||
        p.endsWith(".csv") ||
        p.endsWith(".tsv") ||
        p.endsWith(".txt");
      if (!isDocOrTable) {
        next.add(e.path);
      }
    }
    setExcluded(next);
  };

  const applyDropOver = () => {
    if (!scanReport) return;
    const limit = (parseFloat(dropOverMb) || 0) * 1048576;
    if (limit <= 0) return;
    const next = new Set(excluded);
    for (const e of scanReport.included) if (e.size > limit) next.add(e.path);
    setExcluded(next);
  };

  // PDFs and saved web pages/captures (MHTML, HAR, WARC, Monad Crawl
  // network_requests.json): 15 % of the 561 MB test merge.
  const applyDropWeb = () => {
    if (!scanReport) return;
    const next = new Set(excluded);
    for (const e of scanReport.included) {
      const p = e.path.toLowerCase();
      if (/\.(pdf|mhtml|mht|har|warc)$/.test(p) || p.endsWith("network_requests.json")) next.add(e.path);
    }
    setExcluded(next);
  };

  const handleLoadPlan = async () => {
    const picked = await open({
      multiple: false,
      title: "Load selection plan",
      filters: [{ name: "Selection plan", extensions: ["toml"] }],
    });
    if (typeof picked !== "string") return;
    try {
      const parsed = await invoke<LoadedPlan["plan"]>("read_selection_plan", { path: picked });
      setPlan({ path: picked, plan: parsed });
      setError(null);
    } catch (err) {
      setError(String(err));
    }
  };

  const handleSavePlan = async () => {
    if (!scanReport) return;
    const target = await save({
      title: "Save selection as plan",
      defaultPath: "selection.tmsel.toml",
      filters: [{ name: "Selection plan", extensions: ["toml"] }],
    });
    if (!target) return;
    const files = scanReport.included
      .filter((e) => !excluded.has(e.path))
      .map((e) => [e.path, e.size] as [string, number]);
    try {
      await invoke<number>("write_selection_plan", { path: target, files });
    } catch (err) {
      setError(String(err));
    }
  };

  const openBig = async (path: string, bytes: number) => {
    const ok = await ask(
      `This file is ${formatBytes(bytes)}. A text editor may freeze opening it. Open it anyway?`,
      { title: "Open a large file", kind: "warning" }
    );
    if (ok) openThing(path);
  };

  const applyIncludeAll = () => {
    setExcluded(new Set());
  };

  const handleWatchToggle = async () => {
    if (watching) {
      await invoke("stop_watch").catch(console.error);
      setWatching(false);
      setWatchOutput("");
      return;
    }
    if (!sourcePath || isRemote) return;
    try {
      setError(null);
      const out = await invoke<string>("start_watch", { options: buildOptions() });
      setWatchOutput(out);
      setWatching(true);
    } catch (err) {
      setError(String(err));
      setStatus("error");
    }
  };

  const openThing = (path: string) => invoke("open_file", { path }).catch(console.error);
  const openFolder = (path: string) => invoke("open_folder", { path }).catch(console.error);
  const openWeb = (site: "claude" | "chatgpt" | "gemini") =>
    invoke("open_web", { site }).catch(console.error);

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

  const isWorking = status === "scanning" || status === "merging" || status === "cancelling";
  const formatClock = (ms: number) => {
    const s = Math.round(ms / 1000);
    if (s >= 3600) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
    return s >= 60 ? `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s` : `${s}s`;
  };
  const stageLabel = (p: JobProgress) =>
    p.stage === "scan" ? "Scanning directory..."
      : p.stage === "classify" ? "Classifying files..."
      : p.stage === "ocr" ? `Reading text from pictures (OCR)${p.current ? ` — ${p.current}` : "…"}`
      : p.current;

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
            <input
              type="text"
              className="input"
              value={sourcePath}
              onChange={(e) => changeSource(e.target.value)}
              placeholder="Folder, owner/repo, or GitHub URL — or drag a folder here..."
              disabled={isWorking || watching}
            />
            <button className="btn btn-secondary" onClick={selectSource} disabled={isWorking || watching}>Browse</button>
          </div>
          {autoScanNote && !scanReport && (
            <div style={{ marginTop: 8, fontSize: 13, color: "var(--text-secondary)" }}>{autoScanNote}</div>
          )}
          {scanReport && scanReport.included.length > GUI_CURATE_MAX_FILES && (
            <div style={{ marginTop: 8, fontSize: 13, color: "var(--warning)" }}>
              {scanReport.included.length.toLocaleString()} files: curating here is for up to{" "}
              {GUI_CURATE_MAX_FILES.toLocaleString()}. Above that use the CLI (turbomerger merge --files-from or --selection).
            </div>
          )}
          {scanReport && (
            <>
              <div
                style={{
                  marginTop: 8,
                  padding: "8px 12px",
                  background: "var(--bg-secondary)",
                  borderRadius: 6,
                  border: "1px solid var(--border)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  flexWrap: "wrap",
                  gap: 8,
                }}
              >
                <div style={{ fontSize: 13, color: "var(--text-secondary)" }}>
                  <strong>Files:</strong> {scanReport.included.length - excluded.size} / {scanReport.included.length} |{" "}
                  <strong>Est. Tokens:</strong>{" "}
                  {scanReport.included.some((e) => e.estimated && !excluded.has(e.path)) ? "≈" : ""}
                  {formatTokens(
                    scanReport.included.reduce(
                      (sum, e) => (excluded.has(e.path) ? sum : sum + e.tokens),
                      0
                    )
                  )}{" "}
                  | <strong>Budget:</strong>{" "}
                  {(() => {
                    const budgetLimit = maxTokens ? parseInt(maxTokens, 10) || 0 : 0;
                    const activeTokens = scanReport.included.reduce(
                      (sum, e) => (excluded.has(e.path) ? sum : sum + e.tokens),
                      0
                    );
                    const isOk = budgetLimit === 0 || activeTokens <= budgetLimit;
                    return (
                      <span
                        style={{
                          color: isOk ? "var(--accent)" : "#f85149",
                          fontWeight: 600,
                        }}
                      >
                        {budgetLimit === 0
                          ? "OK"
                          : isOk
                          ? "OK"
                          : `Exceeded (${formatTokens(activeTokens)} / ${formatTokens(budgetLimit)})`}
                      </span>
                    );
                  })()}
                </div>
                <button
                  type="button"
                  className="btn btn-secondary"
                  style={{ padding: "3px 8px", fontSize: 12 }}
                  onClick={() => setCurateOpen(true)}
                  disabled={isWorking}
                >
                  Expand Curate Tree
                </button>
              </div>

              <div
                style={{
                  marginTop: 6,
                  display: "flex",
                  gap: 6,
                  flexWrap: "wrap",
                  alignItems: "center",
                }}
              >
                <span
                  style={{
                    fontSize: 11,
                    color: "var(--text-muted)",
                    textTransform: "uppercase",
                    fontWeight: 600,
                    marginRight: 2,
                  }}
                >
                  Quick Filter:
                </span>
                <button
                  type="button"
                  className="btn btn-secondary"
                  style={{ padding: "3px 8px", fontSize: 12 }}
                  onClick={applyDropDumps}
                  disabled={isWorking}
                  title="Drop JSON > 500KB, SQLite, out/fast/, cache/"
                >
                  Drop Dumps &amp; Caches
                </button>
                <button
                  type="button"
                  className="btn btn-secondary"
                  style={{ padding: "3px 8px", fontSize: 12 }}
                  onClick={applyDropMedia}
                  disabled={isWorking}
                  title="Drop podcast, transcript, audio"
                >
                  Drop Media/Transcripts
                </button>
                <button
                  type="button"
                  className="btn btn-secondary"
                  style={{ padding: "3px 8px", fontSize: 12 }}
                  onClick={applyDropWeb}
                  disabled={isWorking}
                  title="Drop .pdf, .mhtml/.mht, .har, .warc and network_requests.json"
                >
                  Drop PDF / Web Captures
                </button>
                <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                  <button
                    type="button"
                    className="btn btn-secondary"
                    style={{ padding: "3px 8px", fontSize: 12 }}
                    onClick={applyDropOver}
                    disabled={isWorking}
                    title="Drop every file larger than this many MB"
                  >
                    Drop Files Over
                  </button>
                  <input
                    type="number"
                    className="input"
                    style={{ width: 64, padding: "2px 4px", fontSize: 12 }}
                    min="0"
                    value={dropOverMb}
                    onChange={(e) => setDropOverMb(e.target.value)}
                    aria-label="Size limit in MB"
                  />
                  <span style={{ fontSize: 12 }}>MB</span>
                </span>
                <button
                  type="button"
                  className="btn btn-secondary"
                  style={{ padding: "3px 8px", fontSize: 12 }}
                  onClick={applyDocsAndTables}
                  disabled={isWorking}
                  title="Keep only spreadsheets (XLSX, XLSM, XLSB, XLS, ODS), MD, CSV, TSV, TXT"
                >
                  Docs &amp; Tables Only
                </button>
                {excluded.size > 0 && (
                  <button
                    type="button"
                    className="btn btn-secondary"
                    style={{ padding: "3px 8px", fontSize: 12, color: "var(--accent)" }}
                    onClick={applyIncludeAll}
                    disabled={isWorking}
                    title="Reset exclusions"
                  >
                    Include All ({scanReport.included.length})
                  </button>
                )}
              </div>
            </>
          )}
          {isRemote && (
            <>
              <p className="hint">
                Remote repo detected — it will be shallow-cloned to a temp dir, merged, then deleted.
              </p>
              <div className="input-row" style={{ marginTop: 6 }}>
                <input
                  type="password"
                  className="input"
                  value={pat}
                  onChange={(e) => setPat(e.target.value)}
                  placeholder="Personal access token (optional, private repos; kept in memory only)"
                  disabled={isWorking}
                />
              </div>
            </>
          )}
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
            [respectGitignore, setRespectGitignore, "Respect .gitignore / .turbomergerignore (leave ignored files out)"],
            [redactSecrets, setRedactSecrets, "Redact secrets (API keys, tokens, private keys) — can also change code that looks like a secret; check the Merge Report"],
            [extractImagesText, setExtractImagesText, "Text from pictures and scanned PDF pages (OCR — slow)"],
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
          {([
            [ocrAllPictures, setOcrAllPictures, "Also read thumbnails, figure crops and OCR work folders (slow)"],
            [keepFigures, setKeepFigures, "Keep figures (with OCR): crops saved beside the output"],
          ] as [boolean, (b: boolean) => void, string][]).map(([val, setter, label], i) => (
            <div className="checkbox-group" key={`ocr-${i}`} style={{ marginLeft: 24 }}>
              <label className="checkbox-label">
                <input type="checkbox" checked={extractImagesText && val} disabled={isWorking || !extractImagesText}
                  onChange={(e) => { setter(e.target.checked); setPreset("custom"); }} />
                <span>{label}</span>
              </label>
            </div>
          ))}
          <span className="field-label">Size cap: none — every file is taken whatever its size (big ones are streamed)</span>
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
              <div className="checkbox-group">
                <label className="checkbox-label">
                  <input type="checkbox" checked={compress} disabled={isWorking}
                    onChange={(e) => { setCompress(e.target.checked); setPreset("custom"); }} />
                  <span>Compress to signatures (elide function bodies — ~60-80% fewer tokens)</span>
                </label>
              </div>
              <div className="checkbox-group">
                <label className="checkbox-label">
                  <input type="checkbox" checked={stripComments} disabled={isWorking}
                    onChange={(e) => { setStripComments(e.target.checked); setPreset("custom"); }} />
                  <span>Strip comments (tree-sitter)</span>
                </label>
              </div>
              <div className="checkbox-group">
                <label className="checkbox-label">
                  <input type="checkbox" checked={gitDiff} disabled={isWorking}
                    onChange={(e) => { setGitDiff(e.target.checked); setPreset("custom"); }} />
                  <span>Append git diff (working tree vs HEAD) at the end</span>
                </label>
              </div>
              <div className="checkbox-group">
                <label className="checkbox-label">
                  <input type="checkbox" checked={emitSkill} disabled={isWorking}
                    onChange={(e) => { setEmitSkill(e.target.checked); setPreset("custom"); }} />
                  <span>Write .claude/skills/&lt;repo&gt;/SKILL.md into the repo</span>
                </label>
              </div>
              <div style={{ marginTop: 8 }}>
                <span className="field-label">Append git log (last N commits; blank = off)</span>
                <input type="number" className="input" value={gitLogCount} placeholder="e.g. 10" min="0"
                  onChange={(e) => { setGitLogCount(e.target.value); setPreset("custom"); }} disabled={isWorking} />
              </div>
              <div style={{ marginTop: 12 }}>
                <span className="field-label">Same as the CLI (only cutting inside a file stays CLI/plan-only)</span>
                <div style={{ marginTop: 4 }}>
                  <span className="field-label">Stop and continue later (--checkpoint / --resume; needs an output file, not with redaction)</span>
                  <select className="input" value={resumeMode} disabled={isWorking || redactSecrets || isRemote}
                    onChange={(e) => setResumeMode(e.target.value as "off" | "checkpoint" | "resume")}>
                    <option value="off">Off</option>
                    <option value="checkpoint">Keep a checkpoint beside the output</option>
                    <option value="resume">Resume from the checkpoint beside the output</option>
                  </select>
                  {resumeMode !== "off" && !outputPath && (
                    <span style={{ fontSize: 12, color: "#c0392b" }}>Choose an output file: the checkpoint sits beside it.</span>
                  )}
                </div>
                <label className="checkbox-label" style={{ marginTop: 6 }}>
                  <input type="checkbox" checked={completeTwin || (plan?.plan.complete ?? false)} disabled={isWorking || (plan?.plan.complete ?? false)}
                    onChange={(e) => setCompleteTwin(e.target.checked)} />
                  <span>Archive twin: every file rebuildable byte for byte (--complete; Markdown only)</span>
                </label>
                <div style={{ marginTop: 4 }}>
                  <span className="field-label">Embed binaries up to N MB each (--embed-binary; blank = no cap)</span>
                  <input type="number" className="input" value={embedBinaryMb} placeholder="no cap" min="0"
                    disabled={isWorking || !(completeTwin || (plan?.plan.complete ?? false))}
                    onChange={(e) => setEmbedBinaryMb(e.target.value)} />
                </div>
                <div style={{ marginTop: 4 }}>
                  <span className="field-label">Token budget ceiling (--budget-tokens; warns when the output is over)</span>
                  <input type="number" className="input" value={budgetTokens} placeholder="off" min="0"
                    disabled={isWorking} onChange={(e) => setBudgetTokens(e.target.value)} />
                </div>
                <div style={{ marginTop: 4 }}>
                  <span className="field-label">Spreadsheet rows per table chunk (--grid-chunk-rows; blank = one table per sheet)</span>
                  <input type="number" className="input" value={gridChunkRows} placeholder="one table per sheet" min="0"
                    disabled={isWorking} onChange={(e) => setGridChunkRows(e.target.value)} />
                </div>
                <label className="checkbox-label" style={{ marginTop: 6 }}>
                  <input type="checkbox" checked={reproducible} disabled={isWorking} onChange={(e) => setReproducible(e.target.checked)} />
                  <span>Reproducible output: no timestamps or machine paths (--reproducible)</span>
                </label>
                <label className="checkbox-label">
                  <input type="checkbox" checked={showSourcePath} disabled={isWorking} onChange={(e) => setShowSourcePath(e.target.checked)} />
                  <span>Show the source folder path in the header (--show-source-path)</span>
                </label>
              </div>
            </div>
          )}
          {budgetNote && <div style={{ marginTop: 8, fontSize: 13, color: "#c0392b" }}>{budgetNote}</div>}
        </section>

        <section className="section">
          {isWorking ? (
            <button className="btn btn-danger btn-full" onClick={handleCancel} disabled={status === "cancelling" || !jobId}>
              {status === "cancelling" ? "CANCELLING…" : "CANCEL"}
            </button>
          ) : (
            <div className="merge-row">
              <button className="btn btn-primary merge-main" onClick={() => void handleMerge()} disabled={!sourcePath || watching || isWorking}>
                {status === "done" ? "MERGE AGAIN" : status === "error" ? "RETRY" : "MERGE"}
                {plan ? " PLAN" : scanReport && excluded.size > 0 ? " SELECTED" : ""}
              </button>
              <button
                className="btn btn-secondary"
                onClick={() => handleScan(undefined, true)}
                disabled={!sourcePath || isRemote || watching}
                title={isRemote ? "Curation works on local folders" : "Scan first, then curate what gets merged"}
              >
                Scan &amp; curate
              </button>
              <button
                className={`btn ${watching ? "btn-success" : "btn-secondary"}`}
                onClick={handleWatchToggle}
                disabled={!sourcePath || isRemote}
                title="Re-merge automatically when files change"
              >
                {watching ? "◉ Watching" : "Watch"}
              </button>
              <button
                className="btn btn-secondary"
                onClick={() => void handleLoadPlan()}
                disabled={isRemote || isWorking}
                title="Merge exactly the files (and parts) a .tmsel.toml plan lists — the CLI's --selection"
              >
                Load plan…
              </button>
              {scanReport && (
                <button
                  className="btn btn-secondary"
                  onClick={() => void handleSavePlan()}
                  disabled={isWorking}
                  title="Save the curated file list as a .tmsel.toml plan (rebuild it with one click, or with turbomerger merge --selection)"
                >
                  Save as plan…
                </button>
              )}
            </div>
          )}
          {plan && (
            <p className="hint">
              Plan: {plan.path.split(/[\\/]/).pop()} ({(plan.plan.file ?? []).length} files
              {plan.plan.complete ? ", archive twin" : ""}) — MERGE takes exactly these, relative to the source folder
              {" · "}
              <button className="link-btn" onClick={() => setPlan(null)}>clear</button>
            </p>
          )}
          {bigMerge && !isWorking && (
            <div className="checkbox-group" role="alertdialog" aria-label="Big merge">
              <p className="progress-text" style={{ color: "var(--warning)" }}>
                This would write ONE file of about {formatTokens(bigMerge.tokens)} tokens
                ({formatBytes(bigMerge.bytes)}, {bigMerge.files.toLocaleString()} files) — too big for a chat window
                or an editor. Choose:
              </p>
              <button
                className="btn btn-primary"
                onClick={() => {
                  setBigMerge(null);
                  if (scanReport) setCurateOpen(true);
                  else void handleScan(undefined, true);
                }}
              >
                Curate first
              </button>
              <button
                className="btn btn-secondary"
                onClick={() => {
                  setMaxTokens(SPLIT_TOKENS);
                  void handleMerge(true, SPLIT_TOKENS);
                }}
              >
                Split into parts of {parseInt(SPLIT_TOKENS, 10).toLocaleString()} tokens
              </button>
              <button className="btn btn-danger" onClick={() => void handleMerge(true)}>
                Merge anyway
              </button>
            </div>
          )}
          {watching && (
            <p className="watch-banner">
              ◉ Watching — re-merges on change → <span className="output-path-inline">{watchOutput}</span>
            </p>
          )}
        </section>

        {scanReport && curateOpen && (
          <CuratePanel
            report={scanReport}
            excluded={excluded}
            forceInclude={forceInclude}
            budgetTokens={maxTokens ? parseInt(maxTokens, 10) || 200000 : 200000}
            onSetExcluded={setExcluded}
            onToggleForce={(path) => {
              const next = new Set(forceInclude);
              if (next.has(path)) next.delete(path);
              else next.add(path);
              setForceInclude(next);
            }}
            onClose={() => setCurateOpen(false)}
          />
        )}
        {scanReport && !curateOpen && (excluded.size > 0 || forceInclude.size > 0) && (
          <section className="section">
            <p className="hint">
              Curated selection active: {scanReport.included.length - excluded.size} of {scanReport.included.length} files
              {forceInclude.size > 0 && `, +${forceInclude.size} rescued`}
              {" · "}
              <button className="link-btn" onClick={() => setCurateOpen(true)}>edit</button>
              {" · "}
              <button className="link-btn" onClick={() => { setExcluded(new Set()); setForceInclude(new Set()); }}>clear</button>
            </p>
          </section>
        )}

        {isWorking && (
          <section className="section">
            <div className="progress-container">
              <div
                className="progress-bar"
                style={{ width: `${progress && progress.total > 0 ? (progress.done / progress.total) * 100 : 0}%` }}
              />
            </div>
            <p className="status-text">
              {status === "cancelling" ? "Cancelling…" : progress ? stageLabel(progress) : "Scanning directory..."}
            </p>
            {progress && (
              <p className="progress-text">
                {progress.total > 0
                  ? `${progress.done.toLocaleString()} / ${progress.total.toLocaleString()} ${progress.stage === "ocr" ? "pages" : "files"} (${((progress.done / progress.total) * 100).toFixed(1)}%)`
                  : `${progress.done.toLocaleString()} entries`}
                {` · ${formatClock(progress.elapsed_ms)}`}
                {progress.eta_ms !== null && ` · ETA ${formatClock(progress.eta_ms)} (≈ ${finishClock(progress.eta_ms)})`}
                {progress.eta_ms !== null && progress.eta_band_ms !== null && progress.eta_band_ms >= 1000 &&
                  (progress.eta_band_ms * 2 >= progress.eta_ms ? " (rough)" : ` ± ${formatClock(progress.eta_band_ms)}`)}
              </p>
            )}
            {progress && progress.stalled_ms !== null && (
              <p className="progress-text" style={{ color: "var(--warning)" }}>
                Stalled on {progress.current || "the scan"}
                {(() => {
                  const e = scanReport?.included.find((x) => x.path === progress.current);
                  return e ? ` (${formatBytes(e.size)})` : "";
                })()}{" "}
                for {formatClock(progress.stalled_ms)} — Cancel stops it
              </p>
            )}
            {longRun && isWorking && status !== "cancelling" && (
              <div className="checkbox-group" role="alertdialog" aria-label="Long run">
                <p className="progress-text" style={{ color: "var(--warning)" }}>
                  This is a long run:{" "}
                  {progress && progress.eta_ms !== null
                    ? `about ${formatClock(progress.eta_ms)} left (finishes ≈ ${finishClock(progress.eta_ms)})`
                    : `still counting files after ${formatClock(longRun.elapsedMs)}`}
                  . Proceed? The estimate keeps updating.
                </p>
                <button className="btn btn-primary" onClick={() => setLongRun(null)}>PROCEED</button>
                <button className="btn btn-danger" onClick={() => { setLongRun(null); void handleCancel(); }}>
                  CANCEL
                </button>
              </div>
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
            {result.not_captured > 0 && (
              <p className="detection-breakdown" style={{ color: "var(--warning)" }}>
                ⚠ {result.not_captured.toLocaleString()} input{result.not_captured === 1 ? "" : "s"} not
                captured (documents, photos, too large, unreadable…) — see “Not captured” in the Merge Report
              </p>
            )}
            {result.secrets_redacted > 0 && (
              <p className="detection-breakdown" style={{ color: "var(--warning)" }}>
                ⚠ {result.secrets_redacted.toLocaleString()} secret{result.secrets_redacted === 1 ? "" : "s"} redacted — see Merge Report in the output
              </p>
            )}
            {result.output_paths.length > 1 && (
              <p className="detection-breakdown">Split into {result.output_paths.length} parts</p>
            )}
            {result.skill_path && (
              <p className="detection-breakdown" style={{ color: "var(--success)" }}>
                ✓ Claude skill written: {result.skill_path}
              </p>
            )}
            {(result.output_bytes ?? 0) > BIG_OUTPUT_BYTES ? (
              <>
                <p className="detection-breakdown" style={{ color: "var(--warning)" }}>
                  ⚠ {formatBytes(result.output_bytes ?? 0)} written — too big for most editors and chat windows.
                  Use Show in Folder, or merge again with Max tokens set to split it.
                </p>
                <div className="action-buttons">
                  <button className="btn btn-success" onClick={() => openFolder(result.output_path)}>Show in Folder</button>
                  <button className="btn btn-secondary" onClick={() => void openBig(result.output_path, result.output_bytes ?? 0)}>Open File…</button>
                </div>
              </>
            ) : (
              <div className="action-buttons">
                <button className="btn btn-success" onClick={() => openThing(result.output_path)}>Open File</button>
                <button className="btn btn-secondary" onClick={() => openFolder(result.output_path)}>Show in Folder</button>
              </div>
            )}
            <div className="action-buttons">
              <button className="btn btn-secondary" onClick={() => openWeb("claude")}>claude.ai</button>
              <button className="btn btn-secondary" onClick={() => openWeb("chatgpt")}>chatgpt.com</button>
              <button className="btn btn-secondary" onClick={() => openWeb("gemini")}>gemini</button>
            </div>
            {result.output_paths.map((p) => <p className="output-path" key={p}>{p}</p>)}
          </section>
        )}

        <section className="section">
          <button className="link-btn" onClick={() => setShowApply(!showApply)}>
            {showApply ? "▾" : "▸"} Apply an LLM reply back to this folder (T3-3)
          </button>
          {showApply && (
            !sourcePath || isRemote ? (
              <p className="hint">Pick a local source folder above first — apply-back writes into it.</p>
            ) : (
              <ApplyPanel key={sourcePath} root={sourcePath} disabled={isWorking || watching} />
            )
          )}
        </section>

        {error && <section className="section error-section"><p className="error-text">{error}</p></section>}
        {status === "cancelled" && <section className="section cancelled-section"><p className="cancelled-text">Operation cancelled</p></section>}
      </div>

      <footer className="footer"><span>TurboMerger {version ? `v${version}` : ""}</span></footer>
    </div>
  );
}

export default App;
