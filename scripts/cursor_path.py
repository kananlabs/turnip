#!/usr/bin/env python3
"""Simulate cursor-row movement through a captured terminal byte stream.

Reads a raw capture (as produced by `script /dev/null`), tracks the cursor's
row (with screen-height scrolling), and reports the row at which each
occurrence of a marker string is written. A live-region renderer that works
writes its marker at a constant row; a walking/duplicating one advances.
"""
import re
import sys

TOKEN = re.compile(
    rb"\x1b\[(\d*)(A|B|G|K|J)|\r\n|\r|\n",
)

def main(path, marker, rows):
    data = open(path, "rb").read()
    row = 0
    hits = []
    index = 0
    text_start = None
    while index < len(data):
        match = TOKEN.search(data, index)
        if match is None:
            break
        literal = data[index:match.start()]
        if literal and text_start is None:
            text_start = row
        if marker in literal:
            hits.append(row)
        token = match.group(0)
        if token == b"\r\n" or token == b"\n":
            if row >= rows - 1:
                pass  # scroll: content moves up, cursor stays on the last row
            else:
                row += 1
            text_start = None
        elif token == b"\r":
            text_start = None
        else:
            count = match.group(1)
            count = int(count) if count else (1 if match.group(2) in (b"A", b"B") else 0)
            kind = match.group(2)
            if kind == b"A":
                row = max(0, row - count)
            elif kind == b"B":
                row += count
            text_start = None
        index = match.end()
    literal = data[index:]
    if marker in literal:
        hits.append(row)
    return hits

if __name__ == "__main__":
    path, marker, rows = sys.argv[1], sys.argv[2].encode(), int(sys.argv[3])
    hits = main(path, marker, rows)
    print(f"{len(hits)} write(s) of {marker!r}; rows: {hits}")
