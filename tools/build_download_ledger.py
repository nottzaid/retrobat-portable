#!/usr/bin/env python3
"""Pin every DOWNLOAD route to exact bytes and an install recipe.

For each direct-download browse entry this resolves the publisher's file,
downloads it once, and records its URL, size, SHA-256, and how RetroPort must
install it. At runtime RetroPort fetches only these URLs and refuses any byte
that does not match. Entries whose source no longer serves a usable file are
recorded with the reason instead of being silently dropped.

Downloads are cached in ~/.cache/retroport/download-ledger, so reruns only
fetch what changed. Requests are rate-limited per host.

Requirements:
    python -m pip install beautifulsoup4 requests
    7z on PATH (to list 7z/RAR store packages)
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import gzip
import hashlib
import html
import json
import os
import re
import subprocess
import sys
import threading
import time
import unicodedata
import urllib.parse
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path

import requests
from bs4 import BeautifulSoup

ROOT = Path(__file__).resolve().parents[1]
BROWSE = ROOT / "catalog" / "browse-library-v2.json"
OUTPUT = ROOT / "catalog" / "downloads-v1.json.gz"
CACHE = Path(os.environ.get("RETROPORT_LEDGER_CACHE", Path.home() / ".cache/retroport/download-ledger"))
USER_AGENT = "RetroPort download ledger/1.0 (+https://github.com/muradkant/retrobat-portable)"
TIMEOUT = 120

# Must match src/browse_install.rs.
GB_DATABASE = "https://raw.githubusercontent.com/gbdev/database/8a36461e5e2fada5c73484afd87b7e9a9d4e05df"
GBA_DATABASE = "https://raw.githubusercontent.com/gbadev-org/games/9111a814b212318db107a91adb0947b63d1e19a7"
NES_DATABASE = "https://raw.githubusercontent.com/nesdev-org/homebrew-db/95ba342830260e3b7587b5ed230b65f72ec11c2b"
STORE_XML = "https://www.retrobat.ovh/repo/games/store.xml"
STORE_ROOT = "https://www.retrobat.ovh/repo/games/"

# The only hosts a pinned download may come from, per source.
ALLOWED_HOSTS = {
    "homebrew-hub": {"raw.githubusercontent.com"},
    "libretro-content": {"buildbot.libretro.com"},
    "mame-authorized": {"www.mamedev.org"},
    "freedos": {"www.ibiblio.org"},
    "msxdev": {"www.msxdev.org", "msxdev.org"},
    "dos-games-archive": {"www.dosgamesarchive.com"},
    "scummvm-freeware": {"downloads.scummvm.org"},
    "retrobat-store": {"www.retrobat.ovh"},
}

MSX_EXTENSIONS = (".rom", ".mx1", ".mx2", ".dsk", ".cas")


class Unavailable(Exception):
    """The source does not currently serve a usable file for this entry."""


class Fetcher:
    def __init__(self) -> None:
        self.local = threading.local()
        self.host_locks: dict[str, threading.Semaphore] = {}
        self.host_lock = threading.Lock()
        (CACHE / "files").mkdir(parents=True, exist_ok=True)
        (CACHE / "pages").mkdir(parents=True, exist_ok=True)

    def session(self) -> requests.Session:
        if not hasattr(self.local, "session"):
            session = requests.Session()
            session.headers["User-Agent"] = USER_AGENT
            self.local.session = session
        return self.local.session

    def gate(self, url: str) -> threading.Semaphore:
        host = urllib.parse.urlparse(url).hostname or ""
        with self.host_lock:
            return self.host_locks.setdefault(host, threading.Semaphore(2))

    def get(self, url: str, stream: bool = False) -> requests.Response:
        for attempt in range(4):
            with self.gate(url):
                try:
                    response = self.session().get(url, timeout=TIMEOUT, stream=stream)
                except requests.RequestException:
                    if attempt == 3:
                        raise
                    time.sleep(2 ** attempt)
                    continue
                if response.status_code in (429, 500, 502, 503, 504) and attempt < 3:
                    time.sleep(2 ** (attempt + 1))
                    continue
                return response
        raise RuntimeError("unreachable")

    def page(self, url: str) -> str:
        key = hashlib.sha256(url.encode()).hexdigest()
        cached = CACHE / "pages" / key
        if cached.is_file():
            return cached.read_text(encoding="utf-8")
        response = self.get(url)
        if response.status_code != 200:
            raise Unavailable(f"{url} answered HTTP {response.status_code}")
        text = response.content.decode(response.encoding or "utf-8", errors="replace")
        cached.write_text(text, encoding="utf-8")
        return text

    def file(self, url: str) -> Path:
        key = hashlib.sha256(url.encode()).hexdigest()
        cached = CACHE / "files" / key
        if cached.is_file():
            return cached
        response = self.get(url, stream=True)
        if response.status_code != 200:
            raise Unavailable(f"{url} answered HTTP {response.status_code}")
        temporary = cached.with_suffix(f".{os.getpid()}.{threading.get_ident()}.tmp")
        with open(temporary, "wb") as output:
            for chunk in response.iter_content(1 << 20):
                output.write(chunk)
        temporary.replace(cached)
        return cached


def digest(path: Path) -> tuple[int, str]:
    hasher = hashlib.sha256()
    size = 0
    with open(path, "rb") as handle:
        while chunk := handle.read(1 << 20):
            size += len(chunk)
            hasher.update(chunk)
    return size, hasher.hexdigest()


def https_url(base: str, href: str, source_id: str) -> str:
    url = urllib.parse.urljoin(base, html.unescape(href))
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme == "http":
        # Every allowed host serves the same files over HTTPS.
        url = urllib.parse.urlunparse(parsed._replace(scheme="https"))
        parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS[source_id]:
        raise Unavailable(f"download link leaves the publisher: {url}")
    return url


def links(page: str, base: str) -> list[tuple[str, str]]:
    soup = BeautifulSoup(page, "html.parser")
    return [(a["href"], " ".join(a.get_text(" ").split())) for a in soup.find_all("a", href=True)]


def filename_of(url: str) -> str:
    name = urllib.parse.unquote(urllib.parse.urlparse(url).path.rsplit("/", 1)[-1])
    if not name or "/" in name or "\\" in name or name in {".", ".."}:
        raise Unavailable(f"unusable filename in {url}")
    return name


def normalized(value: str) -> str:
    value = unicodedata.normalize("NFKD", value).encode("ascii", "ignore").decode().lower()
    return re.sub(r"[^a-z0-9]+", "", value)


def real_name(info: zipfile.ZipInfo) -> str:
    """Many archivers store UTF-8 names without setting the ZIP UTF-8 flag,
    which Python then decodes as CP437."""
    name = info.filename
    if not info.flag_bits & 0x800:
        try:
            name = name.encode("cp437").decode("utf-8")
        except (UnicodeEncodeError, UnicodeDecodeError):
            pass
    return name


def zip_files(source) -> dict[str, bytes | None]:
    """Every file in a ZIP, descending one level into inner ZIPs (several
    publishers wrap the game archive in a distribution archive)."""
    files: dict[str, bytes | None] = {}
    try:
        with zipfile.ZipFile(source) as archive:
            for info in archive.infolist():
                if info.is_dir():
                    continue
                name = real_name(info)
                files[name] = None
                if name.lower().endswith(".zip"):
                    try:
                        with zipfile.ZipFile(archive.open(info)) as inner:
                            for inner_info in inner.infolist():
                                if not inner_info.is_dir():
                                    files[f"{name}/{real_name(inner_info)}"] = None
                    except zipfile.BadZipFile:
                        pass
    except zipfile.BadZipFile:
        return {}
    return files


def zip_members(path: Path) -> list[str]:
    return list(zip_files(path))


def zip_member_digest(path: Path, wanted: str) -> tuple[int, str]:
    with zipfile.ZipFile(path) as archive:
        by_name = {real_name(info): info for info in archive.infolist()}
        if wanted in by_name:
            data = archive.read(by_name[wanted])
            return len(data), hashlib.sha256(data).hexdigest()
        for name, info in by_name.items():
            if wanted.startswith(name + "/") and name.lower().endswith(".zip"):
                with zipfile.ZipFile(archive.open(info)) as inner:
                    inner_by_name = {real_name(i): i for i in inner.infolist()}
                    rest = wanted[len(name) + 1:]
                    if rest in inner_by_name:
                        data = inner.read(inner_by_name[rest])
                        return len(data), hashlib.sha256(data).hexdigest()
    raise Unavailable(f"archive member vanished: {wanted}")


def archive_members(path: Path) -> list[str]:
    members = zip_members(path)
    if members:
        return members
    listing = subprocess.run(["7z", "l", "-slt", "-ba", str(path)], capture_output=True, text=True)
    if listing.returncode != 0:
        return []
    members, current, folder = [], None, False
    for line in listing.stdout.splitlines() + [""]:
        if line.startswith("Path = "):
            current = line[7:].replace("\\", "/")
        elif line.startswith("Folder = "):
            folder = line[9:].strip() == "+"
        elif not line.strip() and current is not None:
            if not folder:
                members.append(current)
            current, folder = None, False
    return members


# ---------------------------------------------------------------- sources


def homebrew_hub(fetcher: Fetcher, entry: dict) -> dict:
    slug = entry["id"].split("/", 1)[1]
    base = {"gba": GBA_DATABASE, "nes": NES_DATABASE}.get(entry["system"], GB_DATABASE)
    metadata = json.loads(fetcher.page(f"{base}/entries/{urllib.parse.quote(slug)}/game.json"))
    files = metadata.get("files", [])
    chosen = next((f for f in files if f.get("default") and f.get("playable")), None) or next(
        (f for f in files if f.get("playable")), None
    )
    if not chosen:
        raise Unavailable("Homebrew Hub lists no playable file")
    url = f"{base}/entries/{urllib.parse.quote(slug)}/{urllib.parse.quote(chosen['filename'])}"
    return {"url": url, "filename": filename_of(url), "recipe": "file"}


# Libretro content whose game is a folder of data files: the archive is
# unpacked as it is and RetroPort starts the first member matching these
# preferences (a file name, or an extension), in order.
CONTENT_LAUNCH = {
    "tic-80": ["-lua.tic", ".tic"],
    "dreamcast": [".cdi", ".gdi", ".chd", ".cue"],
    "doom": ["doom1.wad", "doom.wad", "doom2.wad", ".wad"],
    "quake": ["pak0.pak", ".pak"],
    "quake-ii": ["baseq2/pak0.pak", ".pak"],
    "cavestory": ["doukutsu.exe", ".exe"],
    "dinothawr": [".game"],
    "super-bros-war": [".game"],
    "tomb-raider": [".psx", ".phd", ".tr2"],
    "pocketcdg": [".cdg"],
    "wolfenstein-3d": ["vswap.wl6", "vswap.wl1", "vswap.sod", "vswap.sdm", "vswap.n3d"],
}
# Content that cannot be played from what its archive holds.
CONTENT_UNAVAILABLE = {
    "cannonball": "CannonBall is an engine for Sega's OutRun arcade ROMs, which the archive does not include",
    "jump-n-bump": "the Jump 'n Bump core is not part of this RetroBat runtime",
    "rick-dangerous": "RetroBat has no system that starts the xrick core this game needs",
}
CONTENT_ADDONS = {
    "libretro-content/quake-quake-colored-lighting-pack-zip": "a coloured-lighting add-on for Quake, not a game",
    "libretro-content/doom-prboom-wad": "PrBoom's own resource file, not a game (the Doom cards include it)",
    "libretro-content/wolfenstein-3d-ecwolf-pk3": "ECWolf's own resource file, not a game (RetroBat ships it)",
    "libretro-content/easyrpg-wildmidi-zip": "EasyRPG's MIDI instrument set, not a game",
}


def libretro_content(fetcher: Fetcher, entry: dict) -> dict:
    if entry["id"] in CONTENT_ADDONS:
        raise Unavailable(CONTENT_ADDONS[entry["id"]])
    if entry["system"] in CONTENT_UNAVAILABLE:
        raise Unavailable(CONTENT_UNAVAILABLE[entry["system"]])
    url = entry["detail_url"]
    https_url(url, url, "libretro-content")
    plan = {"url": url, "filename": filename_of(url), "recipe": "file"}
    if entry["system"] in CONTENT_LAUNCH and plan["filename"].lower().endswith((".zip", ".7z")):
        plan.update(recipe="extract_tree", launch_preferences=CONTENT_LAUNCH[entry["system"]])
    return plan


def choose_launch(members: list[str], preferences: list[str]) -> str:
    for preference in preferences:
        matches = sorted(
            (m for m in members if m.lower().endswith(preference)),
            key=lambda m: (m.count("/"), m.lower()),
        )
        if matches:
            return matches[0]
    raise Unavailable(f"archive has no file to start ({', '.join(preferences)})")


def mame_authorized(fetcher: Fetcher, entry: dict) -> dict:
    # MAMEdev usually publishes <machine>.zip, but some titles are released
    # only as a variant set (falcnwlda.zip); the page is authoritative.
    machine = entry["id"].split("/", 1)[1]
    page_url = f"https://www.mamedev.org/roms/{machine}/"
    zips = [href for href, _ in links(fetcher.page(page_url), page_url) if href.lower().endswith(".zip")]
    if not zips:
        raise Unavailable("MAMEdev's page links no ROM set")
    href = f"{machine}.zip" if f"{machine}.zip" in zips else zips[0]
    url = https_url(page_url, href, "mame-authorized")
    return {"url": url, "filename": filename_of(url), "recipe": "file"}


def freedos(fetcher: Fetcher, entry: dict) -> dict:
    page_url = entry["detail_url"]
    for href, _ in links(fetcher.page(page_url), page_url):
        if href.lower().endswith(".zip") and "/games/" in href:
            url = https_url(page_url, href, "freedos")
            return {"url": url, "filename": filename_of(url), "recipe": "file"}
    raise Unavailable("FreeDOS package page links no package ZIP")


def dos_games_archive(fetcher: Fetcher, entry: dict) -> dict:
    detail = entry["detail_url"]
    file_page = next(
        (urllib.parse.urljoin(detail, href) for href, _ in links(fetcher.page(detail), detail)
         if href.startswith("/file/") and not href.endswith(".php")),
        None,
    )
    if not file_page:
        raise Unavailable("DOS Games Archive page links no file page")
    page = fetcher.page(file_page)
    download = next((href for href, _ in links(page, file_page) if href.startswith("/file.php?id=")), None)
    heading = re.search(r'<h1 class="download">Download (.+?)</h1>', page)
    if not download or not heading:
        raise Unavailable("DOS Games Archive file page has no download")
    name = html.unescape(heading.group(1)).strip()
    if not name.lower().endswith(".zip") or "/" in name or "\\" in name:
        raise Unavailable(f"DOS Games Archive file is not a ZIP: {name}")
    return {"url": https_url(file_page, download, "dos-games-archive"), "filename": name, "recipe": "file"}


def version_key(text: str) -> tuple:
    numbers = re.findall(r"\d+", text)
    return tuple(int(number) for number in numbers) if numbers else (0,)


def scummvm(fetcher: Fetcher, entry: dict) -> dict:
    page_url, _, fragment = entry["detail_url"].partition("#")
    page = fetcher.page(page_url)
    start = page.find(f'id="{fragment}"')
    if start < 0:
        raise Unavailable("ScummVM games page no longer lists this game")
    section = page[start: page.find('<div class="subhead"', start + 1) % (len(page) + 1)]
    candidates = []
    for match in re.finditer(r'<a href="(https://downloads\.scummvm\.org/[^"]+\.zip)">([^<]*)</a>(.{0,600}?)</li>', section, re.S):
        sha = re.search(r"\b([0-9a-f]{64})\b", match.group(3))
        version = re.search(r"[Vv]ersion\s*v?([\w.]+)", match.group(2))
        candidates.append((version_key(version.group(1) if version else match.group(1)), match.group(1), sha.group(1) if sha else None))
    if not candidates:
        raise Unavailable("ScummVM lists no ZIP for this game")
    _, url, published = max(candidates)
    engine, _, game = fragment.removeprefix("games-").partition(":")
    return {
        "url": https_url(page_url, url, "scummvm-freeware"),
        "filename": filename_of(url),
        "recipe": "scummvm",
        "game_id": f"{engine}:{game or engine}",
        "published_sha256": published,
    }


def msxdev(fetcher: Fetcher, entry: dict) -> dict:
    detail = entry["detail_url"]
    page_url, _, _ = detail.partition("#")
    zips = []
    for href, _ in links(fetcher.page(page_url), page_url):
        if "/wp-content/uploads/" in href and href.lower().endswith(".zip"):
            try:
                zips.append(https_url(page_url, href, "msxdev"))
            except Unavailable:
                continue
    if not zips:
        raise Unavailable("MSXdev page links no ZIP")
    year = entry.get("release_year") or 0
    if year >= 2021:
        return {"url": zips[0], "filename": filename_of(zips[0]), "recipe": "extract_member"}
    # 2003-2020 edition pages list every entry together, as one compilation
    # or as one archive per game; locate this title among them.
    return {"url": None, "candidates": zips, "filename": None, "recipe": "extract_member", "title": entry["title"]}


def retrobat_store(fetcher: Fetcher, entry: dict, store: dict) -> dict:
    local = entry["id"].split("/", 1)[1]
    package = store.get(local)
    if not package:
        raise Unavailable("RetroBat's store no longer lists this package")
    url = STORE_ROOT + urllib.parse.quote(package["name"]) + ".7z"
    return {
        "url": url,
        "filename": package["name"] + ".7z",
        "recipe": "retrobat_store",
        "member": f"roms/{package['system']}/{package['path']}",
        "store_system": package["system"],
    }


def slug_ascii(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")


def load_store(fetcher: Fetcher) -> dict:
    root = ET.fromstring(fetcher.get(STORE_XML).content)
    packages = {}
    for package in root.findall("package"):
        game = package.find("game")
        name = (package.findtext("name") or "").strip()
        if game is None or not name:
            continue
        path = (game.findtext("path") or "").strip().removeprefix("./")
        system = (game.get("system") or "").strip().lower()
        packages[slug_ascii(name)] = {"name": name, "system": system, "path": path}
    return packages


# ---------------------------------------------------------------- recipes


def title_distance(title: str, archive: str, member: str) -> int | None:
    """How well an archive/member names this title (lower is better)."""
    wanted = normalized(title)
    parts = member.split("/")
    names = [normalized(Path(part).stem) for part in parts[:-1]]
    names += [normalized(Path(archive).stem), normalized(Path(member).stem)]
    for name in names:
        name = re.sub(r"^(msxdev\d\d|\d+)", "", name)
        if name and (name == wanted or name in wanted or wanted in name):
            return abs(len(name) - len(wanted))
    return None


def choose_msx_member(members: list[str], title: str | None, archive: str = "") -> str:
    playable = [m for m in members if m.lower().endswith(MSX_EXTENSIONS) and "/source/" not in m.lower()
                and "pack" not in Path(m).parent.name.lower()]
    if title is not None:
        scored = []
        for member in playable:
            distance = title_distance(title, archive, member)
            if distance is not None:
                scored.append((distance, member.count("/"), len(member), member))
        if not scored:
            raise Unavailable("the edition's archives have no file identifiable as this title")
        scored.sort()
        best = scored[0][0]
        playable = [s[3] for s in scored if s[0] == best]
    if not playable:
        raise Unavailable("archive contains no MSX ROM, disk, or tape image")
    english = [m for m in playable if re.search(r"(^|[._ -])(en|eng|english)([._ -]|$)", m.lower())]
    ranked = sorted(english or playable, key=lambda m: (not m.lower().endswith(".rom"), m.count("/"), m.lower()))
    return ranked[0]


def finish(fetcher: Fetcher, entry: dict, plan: dict) -> dict:
    if plan["recipe"] == "extract_member" and plan.get("url") is None:
        title = plan.pop("title")
        best = None
        for candidate in plan.pop("candidates"):
            path = fetcher.file(candidate)
            try:
                member = choose_msx_member(zip_members(path), title, filename_of(candidate))
            except Unavailable:
                continue
            distance = title_distance(title, filename_of(candidate), member)
            if best is None or distance < best[0]:
                best = (distance, candidate, member)
        if best is None:
            raise Unavailable("the edition's archives have no file identifiable as this title")
        plan.update(url=best[1], filename=filename_of(best[1]), member=best[2])
    path = fetcher.file(plan["url"])
    size, sha256 = digest(path)
    plan.update(size=size, sha256=sha256)
    published = plan.pop("published_sha256", None)
    if published and published != sha256:
        raise Unavailable(f"bytes differ from the SHA-256 the publisher lists ({published})")
    members = archive_members(path) if plan["filename"].lower().endswith((".zip", ".7z", ".rar")) else []
    if plan["recipe"] == "extract_member":
        if "member" not in plan:
            plan["member"] = choose_msx_member(members, None)
        # Installs locate the member by content, so archive name encodings
        # never decide which file becomes the game.
        plan["member_size"], plan["member_sha256"] = zip_member_digest(path, plan["member"])
    if plan["recipe"] == "extract_tree":
        plan["member"] = choose_launch(members, plan.pop("launch_preferences"))
    if plan["recipe"] == "retrobat_store":
        # Windows ignores case; Linux does not. Record the archive's spelling.
        actual = next((m for m in members if m.lower() == plan["member"].lower()), None)
        if actual is None:
            raise Unavailable(f"store package lacks its declared game file {plan['member']}")
        plan["member"] = actual
    if plan["recipe"] == "file" and entry["system"] == "handheld-electronic-game":
        if any(member.lower().endswith(".mgw") for member in members):
            plan["core"] = "gw"
    plan.pop("store_system", None)
    return plan


def resolve(fetcher: Fetcher, entry: dict, store: dict) -> dict:
    source = entry["source_id"]
    plan = {
        "homebrew-hub": lambda: homebrew_hub(fetcher, entry),
        "libretro-content": lambda: libretro_content(fetcher, entry),
        "mame-authorized": lambda: mame_authorized(fetcher, entry),
        "freedos": lambda: freedos(fetcher, entry),
        "dos-games-archive": lambda: dos_games_archive(fetcher, entry),
        "scummvm-freeware": lambda: scummvm(fetcher, entry),
        "msxdev": lambda: msxdev(fetcher, entry),
        "retrobat-store": lambda: retrobat_store(fetcher, entry, store),
    }[source]()
    return finish(fetcher, entry, plan)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=OUTPUT)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--only", help="limit to one source id (for inspection; does not write)")
    parser.add_argument(
        "--refresh",
        help="re-resolve one source id and merge it into the existing ledger, leaving every other source's records unchanged",
    )
    arguments = parser.parse_args()

    browse = json.loads(BROWSE.read_text())
    entries = [e for e in browse["entries"] if e["acquisition"] == "direct_download"]
    if arguments.only or arguments.refresh:
        entries = [e for e in entries if e["source_id"] == (arguments.only or arguments.refresh)]
    fetcher = Fetcher()
    store = load_store(fetcher)
    pinned: dict[str, dict] = {}
    unavailable: dict[str, str] = {}
    done = 0

    def work(entry: dict) -> tuple[str, dict | None, str | None]:
        try:
            return entry["id"], resolve(fetcher, entry, store), None
        except Unavailable as error:
            return entry["id"], None, str(error)
        except (requests.RequestException, OSError, ValueError, KeyError, ET.ParseError) as error:
            return entry["id"], None, f"source could not be read: {error}"

    with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.workers) as pool:
        for entry_id, plan, reason in pool.map(work, entries):
            done += 1
            if plan:
                pinned[entry_id] = plan
            else:
                unavailable[entry_id] = reason
            if done % 100 == 0 or done == len(entries):
                print(f"{done}/{len(entries)} resolved, {len(unavailable)} unavailable", file=sys.stderr, flush=True)

    total = sum(plan["size"] for plan in pinned.values())
    print(f"pinned {len(pinned)} downloads ({total / 2**30:.2f} GiB); {len(unavailable)} unavailable", file=sys.stderr)
    if arguments.only:
        for entry_id, reason in sorted(unavailable.items())[:40]:
            print(f"  {entry_id}: {reason}", file=sys.stderr)
        return 0
    if arguments.refresh:
        previous = json.loads(gzip.decompress(arguments.output.read_bytes()))
        refreshed = {entry["id"] for entry in entries}
        for entry_id, plan in previous["entries"].items():
            if entry_id not in refreshed:
                pinned[entry_id] = plan
        for entry_id, reason in previous["unavailable"].items():
            if entry_id not in refreshed:
                unavailable[entry_id] = reason
    document = {
        "schema_version": 1,
        "generated_at": dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "entries": dict(sorted(pinned.items())),
        "unavailable": dict(sorted(unavailable.items())),
    }
    payload = json.dumps(document, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode()
    arguments.output.write_bytes(gzip.compress(payload, compresslevel=9, mtime=0))
    print(f"wrote {arguments.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
