import json
import os
import time


def atomic_write_json(path, data):
    # write to a temp file, then swap it in, so readers never see a partial file
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
    for attempt in range(20):
        try:
            os.replace(tmp, path)
            return
        except PermissionError:  # Windows: file briefly locked by a reader
            if attempt == 19:
                raise
            time.sleep(0.05)


def read_json(path):
    for attempt in range(20):
        try:
            with open(path) as f:
                return json.load(f)
        except (PermissionError, json.JSONDecodeError):
            if attempt == 19:
                raise
            time.sleep(0.05)