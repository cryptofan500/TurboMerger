"""Archive end-to-end inputs with known tokens (stdlib only).

Usage: python3 -I make_archives.py OUT_DIR
Prints the expected tokens, one per line, on stdout.
"""
import gzip
import io
import sys
import tarfile
import zipfile
from pathlib import Path

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)
expect = []


def text(tok, extra=""):
    expect.append(tok)
    return f"start {tok} {extra}\n".encode()


# 1. small gzip -> inflated inline
(out / "notes.txt.gz").write_bytes(gzip.compress(text("TMARC_GZ_SMALL_4F1")))

# 2. large gzip (> 8 MiB inflated) -> streaming path; tokens at start, middle, end
filler = ("lorem ipsum dolor sit amet " * 3 + "\n") * 120_000  # ~9.8 MB
body = text("TMARC_GZ_BIG_HEAD_2K8") + filler.encode() + text("TMARC_GZ_BIG_MID_7P3") \
    + filler.encode() + text("TMARC_GZ_BIG_TAIL_9W6")
(out / "big.log.gz").write_bytes(gzip.compress(body, compresslevel=1))

# 3. zip: a text member, a nested-dir member, a binary member
with zipfile.ZipFile(out / "bundle.zip", "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("readme.md", text("TMARC_ZIP_TOP_3Q5"))
    z.writestr("inner/deep/config.ini", text("TMARC_ZIP_DEEP_8R2"))
    z.writestr("img/logo.png", b"\x89PNG\r\n\x1a\n" + bytes(range(256)) * 8)


def tar_add(t, name, data):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    t.addfile(info, io.BytesIO(data))


# 4. plain tar and 5. tar.gz
with tarfile.open(out / "pack.tar", "w") as t:
    tar_add(t, "src/main.c", text("TMARC_TAR_6M4"))
with tarfile.open(out / "pack.tgz", "w:gz") as t:
    tar_add(t, "docs/guide.txt", text("TMARC_TGZ_1H7"))

print("\n".join(expect))
