# A.6 generator: python3 a06_build_perf.py ROOT N_FILES N_TOKENS
import os, random, string, sys
root, n_files, n_tokens = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rnd = random.Random(7)
words = ["model","result","table","baseline","method","we","show","that","the",
         "training","loss","figure","section","data","attention","layer","score"]
def prose(kb):
    out, size = [], 0
    while size < kb * 1024:
        line = " ".join(rnd.choice(words) for _ in range(14)) + ".\n"; out.append(line); size += len(line)
    return "".join(out)
for variant in ("corpus_base", "corpus_dense"):
    d = os.path.join(root, variant, "docs"); os.makedirs(d, exist_ok=True); rnd.seed(7)
    for i in range(n_files):
        open(os.path.join(d, f"doc_{i:04d}.md"), "w").write(prose(100))
alphabet = string.ascii_letters + string.digits
lines = ["# Account notes (gmail login + password list)\n"]
for i in range(n_tokens):
    tok = "".join(rnd.choice(alphabet) for _ in range(12)) + "-" + str(rnd.randint(10, 99))
    lines.append(f"user{i}@corp-mail.net : {tok}\n" if i < 5 else f"entry {i} ref {tok}\n")
open(os.path.join(root, "corpus_dense", "docs", "account_notes.md"), "w").writelines(lines)
