"""Meeting data -> standard interchange formats. If the contract loses nothing,
standard scorers give the same numbers on these exports as on raw engine output."""


def _s(ms):
    return f"{ms / 1000:.3f}"


def ctm(md, uri, channel):
    """NIST CTM: <file> <chan> <start> <dur> <word> [<conf>]"""
    return "".join(f"{uri} {channel + 1} {_s(w['start_ms'])} {_s(w['end_ms'] - w['start_ms'])} {w['text']}"
                   + (f" {w['p']}" if "p" in w else "") + "\n"
                   for w in md["words"] if w["channel"] == channel)


def rttm(md, uri, channel):
    """NIST RTTM from speaker_spans."""
    return "".join(f"SPEAKER {uri} 1 {_s(s['start_ms'])} {_s(s['end_ms'] - s['start_ms'])} <NA> <NA> {s['speaker']} <NA> <NA>\n"
                   for s in md["speaker_spans"] if s["channel"] == channel)


def text(md, channel):
    return " ".join(w["text"] for w in md["words"] if w["channel"] == channel)


def webvtt(md):
    labels = {s["id"]: s.get("label", s["id"]) for s in md["speakers"]}

    def ts(ms):
        h, r = divmod(ms, 3600000)
        m, r = divmod(r, 60000)
        return f"{h:02d}:{m:02d}:{r // 1000:02d}.{r % 1000:03d}"

    out = ["WEBVTT", ""]
    words = md["words"]
    pos = {w["id"]: i for i, w in enumerate(words)}
    for t in md["turns"]:
        run = words[pos[t["first_word"]]:pos[t["last_word"]] + 1]
        who = labels.get(t["speaker"], "?") if t["speaker"] else "?"
        out += [t["id"], f"{ts(t['start_ms'])} --> {ts(t['end_ms'])}",
                f"<v {who}>" + " ".join(w["text"] for w in run), ""]
    return "\n".join(out)
