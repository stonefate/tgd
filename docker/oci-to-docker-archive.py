#!/usr/bin/env python3
"""把 Docker 25+ / containerd 的 OCI save 转成旧版 docker load 能导入的 docker-archive。"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tarfile
import tempfile


def _extract(tar: tarfile.TarFile, name: str, dest: str) -> None:
    member = tar.getmember(name)
    src = tar.extractfile(member)
    if src is None:
        raise SystemExit(f"无法读取 {name}")
    parent = os.path.dirname(dest)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(dest, "wb") as out:
        shutil.copyfileobj(src, out)


def convert(src: str, dst: str) -> None:
    with tarfile.open(src, "r") as inn:
        names = inn.getnames()
        if not any(n.startswith("blobs/") for n in names):
            if os.path.abspath(src) != os.path.abspath(dst):
                shutil.copyfile(src, dst)
            return
        raw = inn.extractfile("manifest.json")
        if raw is None:
            raise SystemExit("镜像包缺少 manifest.json")
        manifests = json.loads(raw.read())
        tmp = tempfile.mkdtemp(prefix="tgd-docker-archive-")
        try:
            out_mf = []
            repos: dict[str, dict[str, str]] = {}
            for img in manifests:
                cfg_src = img["Config"]
                cfg_hash = os.path.basename(cfg_src)
                cfg_name = f"{cfg_hash}.json"
                _extract(inn, cfg_src, os.path.join(tmp, cfg_name))
                layers_out = []
                last_id = cfg_hash
                for layer in img["Layers"]:
                    lid = os.path.basename(layer)
                    last_id = lid
                    ldir = os.path.join(tmp, lid)
                    os.makedirs(ldir, exist_ok=True)
                    _extract(inn, layer, os.path.join(ldir, "layer.tar"))
                    with open(os.path.join(ldir, "VERSION"), "w", encoding="ascii") as fh:
                        fh.write("1.0")
                    with open(os.path.join(ldir, "json"), "w", encoding="utf-8") as fh:
                        json.dump({"id": lid}, fh)
                    layers_out.append(f"{lid}/layer.tar")
                tags = img.get("RepoTags") or ["tgd:0.1.20"]
                out_mf.append({"Config": cfg_name, "RepoTags": tags, "Layers": layers_out})
                for tag in tags:
                    name, ver = tag.rsplit(":", 1) if ":" in tag else (tag, "latest")
                    repos.setdefault(name, {})[ver] = last_id
            with open(os.path.join(tmp, "manifest.json"), "w", encoding="utf-8") as fh:
                json.dump(out_mf, fh)
            with open(os.path.join(tmp, "repositories"), "w", encoding="utf-8") as fh:
                json.dump(repos, fh)
            os.makedirs(os.path.dirname(os.path.abspath(dst)) or ".", exist_ok=True)
            with tarfile.open(dst, "w") as out:
                for dirpath, _, files in os.walk(tmp):
                    for filename in files:
                        full = os.path.join(dirpath, filename)
                        out.add(full, os.path.relpath(full, tmp))
        finally:
            shutil.rmtree(tmp)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"用法: {sys.argv[0]} <oci.tar> <docker-archive.tar>")
    convert(sys.argv[1], sys.argv[2])


if __name__ == "__main__":
    main()
