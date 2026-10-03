#!/usr/bin/env python3
"""Build the checked-in, multi-source browse catalogue from upstream indexes.

This script intentionally produces discovery metadata, not an install manifest.
Installable artifacts live in catalog/trusted-v1.json and require a separate
license, archive-layout, size, and SHA-256 audit.

Requirements:
    python -m pip install beautifulsoup4 requests
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import html
import json
import re
import sys
import unicodedata
import urllib.parse
import xml.etree.ElementTree as ET
from pathlib import Path

import requests
from bs4 import BeautifulSoup, Tag


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "catalog" / "browse-library-v2.json"
HOMEBREW_SNAPSHOT = ROOT / "catalog" / "homebrew-hub-browse-v1.json"
USER_AGENT = "RetroPort catalogue builder/0.1 (+https://www.retrobat.org/)"
TIMEOUT = 45


class Fetcher:
    def __init__(self) -> None:
        self.session = requests.Session()
        self.session.headers["User-Agent"] = USER_AGENT
        self.hashes: dict[str, list[str]] = {}

    def get(self, url: str, source_id: str) -> bytes:
        response = self.session.get(url, timeout=TIMEOUT)
        response.raise_for_status()
        body = response.content
        self.hashes.setdefault(source_id, []).append(
            f"{url}\0{hashlib.sha256(body).hexdigest()}"
        )
        return body

    def composite_hash(self, source_id: str) -> str:
        records = sorted(self.hashes.get(source_id, []))
        return hashlib.sha256("\n".join(records).encode()).hexdigest()


def repair_mojibake(value: str) -> str:
    """Undo UTF-8 that an upstream record stored after decoding it as
    Latin-1 or Windows-1252, possibly more than once ("DoppelgÃ¤nger").
    Text that does not decode back to valid UTF-8 is left exactly as is."""
    while any(ord(character) > 127 for character in value):
        for codec in ("latin-1", "cp1252"):
            try:
                repaired = value.encode(codec).decode("utf-8")
                break
            except UnicodeError:
                continue
        else:
            return value
        if repaired == value:
            return value
        value = repaired
    return value


def clean(value: str | None) -> str:
    if not value:
        return ""
    return " ".join(repair_mojibake(html.unescape(value)).split())


def slug(value: str) -> str:
    normalized = unicodedata.normalize("NFKD", value)
    ascii_value = normalized.encode("ascii", "ignore").decode().lower()
    result = re.sub(r"[^a-z0-9]+", "-", ascii_value).strip("-")
    return result or hashlib.sha256(value.encode()).hexdigest()[:16]


def entry(
    source_id: str,
    local_id: str,
    title: str,
    *,
    developer: str = "Unknown",
    system: str = "unknown",
    kind: str = "game",
    tags: list[str] | None = None,
    license_name: str | None = None,
    artwork_url: str | None = None,
    detail_url: str | None = None,
    description: str = "",
    release_year: int | None = None,
    install_state: str = "audit_required",
    acquisition: str = "direct_download",
) -> dict:
    if artwork_url and artwork_url.startswith("http://"):
        artwork_url = "https://" + artwork_url[7:]
    if artwork_url and artwork_url.startswith("www."):
        artwork_url = "https://" + artwork_url
    if detail_url and detail_url.startswith("http://"):
        detail_url = "https://" + detail_url[7:]
    if detail_url and detail_url.startswith("www."):
        detail_url = "https://" + detail_url
    return {
        "id": f"{source_id}/{slug(local_id)}",
        "source_id": source_id,
        "title": clean(title),
        "developer": clean(developer) or "Unknown",
        "system": slug(system) or "unknown",
        "kind": clean(kind) or "game",
        "tags": sorted({clean(tag) for tag in tags or [] if clean(tag)}),
        "license": clean(license_name) or None,
        "artwork_url": artwork_url or None,
        "detail_url": detail_url or None,
        "description": clean(description),
        "release_year": release_year,
        "install_state": install_state,
        "acquisition": acquisition,
    }


def source(
    source_id: str,
    name: str,
    homepage: str,
    summary: str,
    distribution_policy: str,
    snapshot_ref: str,
    entries: list[dict],
) -> dict:
    return {
        "id": source_id,
        "name": name,
        "homepage": homepage,
        "summary": summary,
        "distribution_policy": distribution_policy,
        "snapshot_ref": snapshot_ref,
        "entry_count": len(entries),
    }


def homebrew_hub() -> tuple[dict, list[dict]]:
    original = json.loads(HOMEBREW_SNAPSHOT.read_text())
    entries = []
    for item in original["entries"]:
        converted = dict(item)
        converted["source_id"] = "homebrew-hub"
        converted.pop("repository", None)
        local_id = converted["id"].split("/", 1)[-1]
        converted["detail_url"] = f"https://hh.gbdev.io/game/{local_id}"
        converted["description"] = ""
        converted["release_year"] = None
        converted["acquisition"] = "direct_download"
        entries.append(converted)
    commits = original["source"].get("snapshot_commits", {})
    snapshot_ref = "; ".join(f"{key}={value}" for key, value in sorted(commits.items()))
    metadata = source(
        "homebrew-hub",
        "Homebrew Hub",
        "https://hh.gbdev.io",
        "Community-maintained homebrew database for Nintendo handhelds and NES.",
        "Entries expose source metadata and playable files; each automatic install still requires an artifact-level license and hash audit.",
        snapshot_ref,
        entries,
    )
    return metadata, entries


RETROBAT_SYSTEMS = {
    "msx1": "msx1",
    "windows": "windows",
    "ports": "ports",
    "pcenginecd": "pcenginecd",
    "pcengine": "pcengine",
    "megadrive": "megadrive",
    "mastersystem": "mastersystem",
}


def retrobat_store(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "retrobat-store"
    url = "https://www.retrobat.ovh/repo/games/store.xml"
    root = ET.fromstring(fetcher.get(url, source_id))
    entries = []
    for package in root.findall("package"):
        game = package.find("game")
        if game is None:
            continue
        package_name = clean(package.findtext("name"))
        raw_system = clean(game.get("system"))
        system = RETROBAT_SYSTEMS.get(raw_system.lower(), raw_system.lower())
        artwork = clean(package.findtext("preview_url"))
        detail = clean(package.findtext("url"))
        tags = [
            clean(package.findtext("group")),
            clean(game.findtext("genre")),
            clean(game.findtext("lang")),
            "RetroBat content download",
        ]
        entries.append(
            entry(
                source_id,
                package_name,
                clean(game.findtext("name")) or clean(package.findtext("description")),
                developer=clean(game.findtext("developer")),
                system=system,
                tags=tags,
                artwork_url=artwork,
                detail_url=detail or url,
                description=clean(game.findtext("desc")),
            )
        )
    entries.sort(key=lambda item: (item["title"].casefold(), item["id"]))
    metadata = source(
        source_id,
        "RetroBat Free Content",
        "https://wiki.retrobat.org/navigation/main-menu",
        "The game catalogue used by RetroBat's built-in Updates & Downloads screen.",
        "RetroBat states that its download section contains only unlicensed/free-to-use games or content whose authors permitted distribution.",
        f"store.xml sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


def freedos(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "freedos"
    url = (
        "https://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/"
        "repositories/1.4/html/en/games/index.html"
    )
    soup = BeautifulSoup(fetcher.get(url, source_id), "html.parser")
    entries = []
    for container in soup.select("div.clickable[id]"):
        link = container.find("a", href=True)
        title = container.select_one(".content_package_caption")
        if not link or not title:
            continue
        description = container.select_one(".content_package_description")
        license_tag = container.select_one(".content_package_oss, .content_package_policy")
        version = container.select_one(".version_data1")
        image = container.find("img", src=True)
        detail_url = urllib.parse.urljoin(url, link["href"])
        artwork_url = urllib.parse.urljoin(url, image["src"]) if image else None
        tags = ["FreeDOS package"]
        if version:
            tags.append(f"version {clean(version.get_text())}")
        entries.append(
            entry(
                source_id,
                container["id"],
                title.get_text(" ", strip=True),
                developer="FreeDOS package maintainers",
                system="dos",
                tags=tags,
                license_name=license_tag.get_text(" ", strip=True) if license_tag else None,
                artwork_url=artwork_url,
                detail_url=detail_url,
                description=description.get_text(" ", strip=True) if description else "",
            )
        )
    entries.sort(key=lambda item: item["title"].casefold())
    metadata = source(
        source_id,
        "FreeDOS Games",
        "https://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/",
        "The Games package group in the official FreeDOS 1.4 repository.",
        "Packages publish explicit license metadata; installation needs DOSBox layout and launch-command auditing.",
        f"index sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


def mame(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "mame-authorized"
    url = "https://www.mamedev.org/roms/"
    soup = BeautifulSoup(fetcher.get(url, source_id), "html.parser")
    entries = []
    for cell in soup.select("td"):
        image = cell.find("img", src=True)
        links = cell.find_all("a", href=True)
        if not image or len(links) < 2:
            continue
        title_link = next((link for link in links if clean(link.get_text())), None)
        if title_link is None:
            continue
        title = clean(title_link.get_text())
        text = clean(cell.get_text(" ", strip=True))
        copyright_match = re.search(r"©\s*(\d{4})\s+(.+)$", text)
        year = int(copyright_match.group(1)) if copyright_match else None
        developer = copyright_match.group(2) if copyright_match else "Original rights holder"
        download = title_link["href"]
        entries.append(
            entry(
                source_id,
                urllib.parse.urlparse(download).path or title,
                title,
                developer=developer,
                system="mame",
                tags=["arcade", "creator-authorized", "non-commercial use"],
                license_name="Free non-commercial download from MAMEdev",
                artwork_url=urllib.parse.urljoin(url, image["src"]),
                detail_url=urllib.parse.urljoin(url, download),
                description="Creator-authorized ROM hosted by the MAME project.",
                release_year=year,
            )
        )
    unique = {item["id"]: item for item in entries}
    entries = sorted(unique.values(), key=lambda item: item["title"].casefold())
    metadata = source(
        source_id,
        "MAME Authorized ROMs",
        url,
        "Arcade ROMs released by their original creators for free, non-commercial use.",
        "MAME permits downloads from its site only and forbids third-party rebundling; RetroPort must fetch directly from MAMEdev.",
        f"page sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


def scummvm(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "scummvm-freeware"
    page_url = "https://www.scummvm.org/games/"
    soup = BeautifulSoup(fetcher.get(page_url, source_id), "html.parser")
    navigation = soup.select_one("div.navigation")
    game_links = []
    if navigation:
        parent = next(
            (
                item
                for item in navigation.select(":scope > ul > li")
                if "Game downloads" in clean(item.get_text())
            ),
            None,
        )
        if parent:
            game_links = parent.select(":scope > ul > li > a[href]")

    repo = requests.get(
        "https://api.github.com/repos/scummvm/scummvm-icons/commits/master",
        timeout=TIMEOUT,
        headers={"User-Agent": USER_AGENT},
    )
    repo.raise_for_status()
    icon_commit = repo.json()["sha"]
    tree = requests.get(
        f"https://api.github.com/repos/scummvm/scummvm-icons/git/trees/{icon_commit}?recursive=1",
        timeout=TIMEOUT,
        headers={"User-Agent": USER_AGENT},
    )
    tree.raise_for_status()
    icon_paths = {
        node["path"]
        for node in tree.json()["tree"]
        if node.get("type") == "blob" and node["path"].endswith(".png")
    }
    games_xml_url = (
        "https://raw.githubusercontent.com/scummvm/scummvm-icons/"
        f"{icon_commit}/default/games.xml"
    )
    games_root = ET.fromstring(fetcher.get(games_xml_url, source_id))
    game_meta = {game.get("id"): game.attrib for game in games_root.findall("game")}

    entries = []
    for link in game_links:
        fragment = urllib.parse.urlparse(link["href"]).fragment
        target = fragment.removeprefix("games-")
        engine, _, game_id = target.partition(":")
        game_id = game_id or engine
        meta = game_meta.get(game_id, {})
        icon_candidates = [
            f"default/icons/{meta.get('engine_id', engine)}-{game_id}.png",
            f"default/icons/{game_id}.png",
        ]
        icon_path = next((path for path in icon_candidates if path in icon_paths), None)
        if icon_path is None:
            suffix = f"-{game_id}.png"
            icon_path = next((path for path in icon_paths if path.endswith(suffix)), None)
        artwork_url = (
            "https://raw.githubusercontent.com/scummvm/scummvm-icons/"
            f"{icon_commit}/{urllib.parse.quote(icon_path, safe='/')}"
            if icon_path
            else None
        )
        year_text = meta.get("year", "")
        entries.append(
            entry(
                source_id,
                game_id,
                link.get_text(" ", strip=True),
                developer="Original game authors",
                system="scummvm",
                tags=["adventure", "official ScummVM download", engine],
                license_name="Freeware distribution via ScummVM",
                artwork_url=artwork_url,
                detail_url=urllib.parse.urljoin(page_url, link["href"]),
                description="Game download published by the ScummVM project.",
                release_year=int(year_text) if year_text.isdigit() else None,
            )
        )
    entries.sort(key=lambda item: item["title"].casefold())
    metadata = source(
        source_id,
        "ScummVM Freeware Games",
        page_url,
        "Freeware games and community-engine games published by ScummVM.",
        "Downloads are hosted by ScummVM and include upstream SHA-256 values; archive layout and launcher files still require per-title audits.",
        f"page+icons sha256={fetcher.composite_hash(source_id)}; icons={icon_commit}",
        entries,
    )
    return metadata, entries


def parse_dos_page(body: bytes) -> list[dict]:
    source_id = "dos-games-archive"
    base = "https://www.dosgamesarchive.com"
    soup = BeautifulSoup(body, "html.parser")
    entries = []
    for game in soup.select("div.game_list > div.game"):
        title_link = game.select_one(".game_info h3 a[href^='/download/']")
        image = game.select_one(".game_screenshot img[src]")
        if not title_link:
            continue
        detail_url = urllib.parse.urljoin(base, title_link["href"])
        local_id = title_link["href"].removeprefix("/download/")
        tags = []
        year = None
        licenses = []
        for link in game.select(".game_info p a[href]"):
            href = link["href"]
            text = clean(link.get_text())
            if href.startswith("/year/") and text.isdigit():
                year = int(text)
            elif href.startswith("/license/"):
                licenses.append(text)
            else:
                tags.append(text)
        developer = "Unknown"
        for row in game.select(".game_info tr"):
            heading = row.find("th")
            value = row.find("td")
            if heading and value and clean(heading.get_text()).lower().startswith("dev"):
                developer = clean(value.get_text(" ", strip=True))
        tags.extend(licenses)
        tags.extend(
            clean(node.get_text(" ", strip=True))
            for node in game.select(".game_details .game_detail")
        )
        entries.append(
            entry(
                source_id,
                local_id,
                title_link.get_text(" ", strip=True),
                developer=developer,
                system="dos",
                tags=tags,
                license_name=", ".join(licenses) or None,
                artwork_url=image["src"] if image else None,
                detail_url=detail_url,
                description="Legally downloadable DOS release indexed by DOS Games Archive.",
                release_year=year,
            )
        )
    return entries


def dos_games_archive(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "dos-games-archive"
    first_url = "https://www.dosgamesarchive.com/games"
    first = fetcher.get(first_url, source_id)
    first_soup = BeautifulSoup(first, "html.parser")
    pages = [
        int(match.group(1))
        for link in first_soup.select("a[href*='page=']")
        if (match := re.search(r"[?&]page=(\d+)", link.get("href", "")))
    ]
    last_page = max(pages, default=1)

    def load_page(page: int) -> tuple[int, bytes]:
        return page, fetcher.get(f"{first_url}?page={page}", source_id)

    bodies = {1: first}
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        for page, body in pool.map(load_page, range(2, last_page + 1)):
            bodies[page] = body

    entries = []
    for page in sorted(bodies):
        entries.extend(parse_dos_page(bodies[page]))
    unique = {item["id"]: item for item in entries}
    entries = sorted(unique.values(), key=lambda item: (item["title"].casefold(), item["id"]))
    metadata = source(
        source_id,
        "DOS Games Archive",
        "https://www.dosgamesarchive.com/",
        "Long-running catalogue of legal DOS freeware, shareware, demos, and liberated full versions.",
        "The archive states all downloads are legal; each file still needs a type/license, executable-layout, malware, size, and hash audit before automatic installation.",
        f"{last_page} pages composite sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


MSXDEV_PAGES = {
    2003: "https://www.msxdev.org/msxdev-archive/msxdev03/",
    2004: "https://www.msxdev.org/msxdev-archive/msxdev04/",
    2005: "https://www.msxdev.org/msxdev-archive/msxdev05/",
    2006: "https://www.msxdev.org/msxdev-archive/msxdev06/",
    2007: "https://www.msxdev.org/msxdev-archive/msxdev07/",
    2008: "https://www.msxdev.org/msxdev-archive/msxdev08/",
    2009: "https://www.msxdev.org/msxdev-archive/msxdev09/",
    2010: "https://www.msxdev.org/msxdev-archive/msxdev10/",
    2011: "https://www.msxdev.org/msxdev-archive/msxdev11/",
    2012: "https://www.msxdev.org/msxdev-archive/msxdev12/",
    2013: "https://www.msxdev.org/msxdev-archive/msxdev13/",
    2014: "https://www.msxdev.org/msxdev-archive/msxdev14/",
    2015: "https://www.msxdev.org/msxdev-archive/msxdev15/",
    2017: "https://www.msxdev.org/msxdev-archive/msxdev17/",
    2018: "https://www.msxdev.org/msxdev-archive/msxdev18/",
    2020: "https://www.msxdev.org/msxdev-archive/msxdev20-2/",
    2021: "https://www.msxdev.org/msxdev21/",
    2022: "https://www.msxdev.org/msxdev22/",
    2023: "https://www.msxdev.org/msxdev23/",
    2024: "https://www.msxdev.org/msxdev24/",
    2025: "https://www.msxdev.org/msxdev25/",
}


def msx_heading_image(heading: Tag, page_url: str) -> str | None:
    for node in heading.find_all_next():
        if node is not heading and node.name in {"h1", "h2", "h3"}:
            break
        if node.name == "img" and node.get("src"):
            candidate = urllib.parse.urljoin(page_url, node["src"])
            if "/wp-content/uploads/" in candidate:
                return candidate
    return None


def msx_old_entries(year: int, page_url: str, soup: BeautifulSoup) -> list[dict]:
    source_id = "msxdev"
    headings = soup.select("h1, h2, h3")
    numbered = [
        heading
        for heading in headings
        if re.match(r"^\s*#?\d{1,2}\s*[-–—.]\s*", clean(heading.get_text()))
    ]
    candidates = numbered
    if not candidates:
        start = next(
            (
                index
                for index, heading in enumerate(headings)
                if "game entries" in clean(heading.get_text()).casefold()
            ),
            None,
        )
        candidates = []
        if start is not None:
            for heading in headings[start + 1 :]:
                text = clean(heading.get_text())
                if re.search(r"results|winners|rules|contestants|jury", text, re.I):
                    break
                if heading.name in {"h2", "h3"} and text:
                    candidates.append(heading)

    entries = []
    for index, heading in enumerate(candidates, start=1):
        raw_title = clean(heading.get_text())
        title = re.sub(r"^\s*#?\d{1,2}\s*[-–—.]\s*", "", raw_title)
        if not title or re.search(r"game entries|category", title, re.I):
            continue
        detail_link = heading.find("a", href=True)
        detail_url = (
            urllib.parse.urljoin(page_url, detail_link["href"])
            if detail_link
            else f"{page_url}#{heading.get('id', slug(title))}"
        )
        nearby = []
        for node in heading.find_all_next(["p", "h1", "h2", "h3"], limit=4):
            if node.name in {"h1", "h2", "h3"} and node is not heading:
                break
            if node.name == "p":
                nearby.append(clean(node.get_text(" ", strip=True)))
        nearby_text = " ".join(nearby)
        developer_match = re.search(r"\bby\s+([^.;]+)", nearby_text, re.I)
        entries.append(
            entry(
                source_id,
                f"{year}-{index:02d}-{title}",
                title,
                developer=developer_match.group(1) if developer_match else "MSXdev entrant",
                system="msx1",
                tags=[f"MSXdev{str(year)[-2:]}", "competition", "freeware"],
                license_name="Freeware; author retains ownership",
                artwork_url=msx_heading_image(heading, page_url),
                detail_url=detail_url,
                description=nearby_text[:600],
                release_year=year,
            )
        )
    return entries


def msx_recent_stubs(year: int, page_url: str, soup: BeautifulSoup) -> list[dict]:
    submitted = next(
        (
            heading
            for heading in soup.select("h2, h3")
            if "submitted games" in clean(heading.get_text()).casefold()
        ),
        None,
    )
    if not submitted:
        return []
    listing = submitted.find_next("ul")
    if not listing:
        return []
    stubs = []
    for index, item in enumerate(listing.find_all("li", recursive=False), start=1):
        link = item.find("a", href=True)
        if not link:
            continue
        text = clean(item.get_text(" ", strip=True))
        developer_match = re.search(r"\s[–—-]\s+by\s+(.+?)(?:\s+\(|$)", text, re.I)
        stubs.append(
            {
                "index": index,
                "title": clean(link.get_text()),
                "developer": developer_match.group(1) if developer_match else "MSXdev entrant",
                "url": urllib.parse.urljoin(page_url, link["href"]),
                "disqualified": "disqualified" in text.casefold(),
                "year": year,
            }
        )
    return stubs


def msxdev(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "msxdev"
    entries = []
    recent_stubs = []
    for year, page_url in MSXDEV_PAGES.items():
        soup = BeautifulSoup(fetcher.get(page_url, source_id), "html.parser")
        if year >= 2021:
            recent_stubs.extend(msx_recent_stubs(year, page_url, soup))
        else:
            entries.extend(msx_old_entries(year, page_url, soup))

    def load_recent(stub: dict) -> dict:
        soup = BeautifulSoup(fetcher.get(stub["url"], source_id), "html.parser")
        image = soup.select_one("meta[property='og:image'][content]")
        description = soup.select_one("meta[property='og:description'][content]")
        tags = [f"MSXdev{str(stub['year'])[-2:]}", "competition", "freeware"]
        if stub["disqualified"]:
            tags.append("disqualified entry")
        return entry(
            source_id,
            f"{stub['year']}-{stub['index']:02d}-{stub['title']}",
            stub["title"],
            developer=stub["developer"],
            system="msx1",
            tags=tags,
            license_name="Freeware; author retains ownership",
            artwork_url=image["content"] if image else None,
            detail_url=stub["url"],
            description=description["content"] if description else "",
            release_year=stub["year"],
        )

    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        entries.extend(pool.map(load_recent, recent_stubs))
    unique = {item["id"]: item for item in entries}
    entries = sorted(
        unique.values(),
        key=lambda item: (item["release_year"] or 0, item["title"].casefold()),
        reverse=True,
    )
    metadata = source(
        source_id,
        "MSXdev",
        "https://www.msxdev.org/msxdev-archive/",
        "Games submitted to the long-running MSXdev development contests.",
        "MSXdev states submitted games are freeware, free to download and play, while authors retain ownership.",
        f"{len(MSXDEV_PAGES)} editions composite sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


LIBRETRO_EXCLUDED_FOLDERS = {"Images", "Utilities", "Video"}
LIBRETRO_EXTENSIONS = {
    ".7z",
    ".a26",
    ".arduboy",
    ".bin",
    ".ch8",
    ".col",
    ".dsk",
    ".game",
    ".gb",
    ".gba",
    ".gg",
    ".hex",
    ".int",
    ".iso",
    ".lutro",
    ".md",
    ".nes",
    ".p8",
    ".pak",
    ".pce",
    ".pk3",
    ".rom",
    ".sfc",
    ".sms",
    ".tap",
    ".tic",
    ".vec",
    ".vboy",
    ".wad",
    ".ws",
    ".zip",
}


LIBRETRO_SYSTEM_MAP = {
    "Arcade": "mame",
    "Arduous": "arduboy",
    "Atari - 2600": "atari2600",
    "Bandai - WonderSwan Color": "wswan",
    "Cave Story": "ports",
    "Coleco - Colecovision": "colecovision",
    "DOS": "dos",
    "GCE - Vectrex": "vectrex",
    "Nintendo - GameBoy": "gb",
    "Nintendo - GameBoy Advance": "gba",
    "Nintendo - Nintendo Entertainment System": "nes",
    "Nintendo - Super Nintendo Entertainment System": "snes",
    "ScummVM": "scummvm",
    "Sega - Dreamcast": "dreamcast",
    "Sega - Game Gear": "gamegear",
    "Sega - Master System - Mark III": "mastersystem",
    "Sega - Mega Drive - Genesis": "megadrive",
    "Sony - PlayStation": "psx",
}


def libretro_content(fetcher: Fetcher) -> tuple[dict, list[dict]]:
    source_id = "libretro-content"
    root_url = "https://buildbot.libretro.com/assets/cores/"
    root = BeautifulSoup(fetcher.get(root_url, source_id), "html.parser")
    folders = []
    for link in root.select("a[href]"):
        href = link["href"]
        if not href.startswith("/assets/cores/") or not href.endswith("/"):
            continue
        folder = urllib.parse.unquote(href.rstrip("/").rsplit("/", 1)[-1])
        if folder and folder not in LIBRETRO_EXCLUDED_FOLDERS:
            folders.append((folder, urllib.parse.urljoin(root_url, href)))
    folders = sorted(set(folders))

    def load_folder(item: tuple[str, str]) -> tuple[str, str, bytes]:
        folder, url = item
        return folder, url, fetcher.get(url, source_id)

    entries = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        pages = list(pool.map(load_folder, folders))
    for folder, page_url, body in pages:
        soup = BeautifulSoup(body, "html.parser")
        for link in soup.select("a[href]"):
            href = link["href"]
            path = urllib.parse.urlparse(href).path
            suffix = Path(urllib.parse.unquote(path)).suffix.casefold()
            if suffix not in LIBRETRO_EXTENSIONS:
                continue
            filename = urllib.parse.unquote(path.rsplit("/", 1)[-1])
            title = Path(filename).stem
            tags = ["RetroArch Content Downloader", folder]
            if "demo" in title.casefold() or "preview" in title.casefold():
                tags.append("demo")
            entries.append(
                entry(
                    source_id,
                    f"{folder}/{filename}",
                    title,
                    developer="Community content authors",
                    system=LIBRETRO_SYSTEM_MAP.get(folder, folder),
                    tags=tags,
                    detail_url=urllib.parse.urljoin(page_url, href),
                    description=f"Content published through Libretro's {folder} downloader.",
                )
            )
    unique = {item["id"]: item for item in entries}
    entries = sorted(unique.values(), key=lambda item: (item["title"].casefold(), item["id"]))
    metadata = source(
        source_id,
        "Libretro Content Downloader",
        root_url,
        "Game, demo, and engine content distributed by RetroArch's maintained buildbot.",
        "Content is published for direct use by RetroArch; individual licenses and archive layouts must be audited before RetroPort installation.",
        f"{len(folders)} folders composite sha256={fetcher.composite_hash(source_id)}",
        entries,
    )
    return metadata, entries


def validate(sources: list[dict], entries: list[dict]) -> None:
    source_ids = [item["id"] for item in sources]
    if len(source_ids) != len(set(source_ids)):
        raise ValueError("duplicate source id")
    entry_ids = [item["id"] for item in entries]
    if len(entry_ids) != len(set(entry_ids)):
        duplicates = sorted({item for item in entry_ids if entry_ids.count(item) > 1})
        raise ValueError(f"duplicate entry ids: {duplicates[:10]}")
    known_sources = set(source_ids)
    for item in entries:
        if item["source_id"] not in known_sources:
            raise ValueError(f"unknown source for {item['id']}")
        if not item["title"]:
            raise ValueError(f"empty title for {item['id']}")
        for field in ("artwork_url", "detail_url"):
            value = item[field]
            if value and urllib.parse.urlparse(value).scheme != "https":
                raise ValueError(f"insecure {field} for {item['id']}: {value}")
    counts = {source_id: 0 for source_id in source_ids}
    for item in entries:
        counts[item["source_id"]] += 1
    for item in sources:
        if item["entry_count"] != counts[item["id"]]:
            raise ValueError(f"entry count mismatch for {item['id']}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=OUTPUT)
    parser.add_argument(
        "--generated-at",
        default=dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    )
    arguments = parser.parse_args()

    fetcher = Fetcher()
    builders = [
        homebrew_hub,
        lambda: retrobat_store(fetcher),
        lambda: scummvm(fetcher),
        lambda: freedos(fetcher),
        lambda: mame(fetcher),
        lambda: msxdev(fetcher),
        lambda: libretro_content(fetcher),
        lambda: dos_games_archive(fetcher),
    ]
    sources = []
    entries = []
    for builder in builders:
        metadata, additions = builder()
        print(f"{metadata['name']}: {len(additions)}", file=sys.stderr)
        sources.append(metadata)
        entries.extend(additions)
    entries.sort(key=lambda item: (item["source_id"], item["title"].casefold(), item["id"]))
    validate(sources, entries)
    document = {
        "schema_version": 2,
        "generated_at": arguments.generated_at,
        "sources": sources,
        "entries": entries,
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(document, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(
        f"Wrote {len(entries)} entries from {len(sources)} sources to {arguments.output}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
