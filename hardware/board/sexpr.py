"""Minimal S-expression reader/writer for KiCad files."""


class Sym(str):
    """A bare (unquoted) token."""


def parse(text):
    pos = 0
    n = len(text)

    def read():
        nonlocal pos
        while pos < n and text[pos].isspace():
            pos += 1
        c = text[pos]
        if c == "(":
            pos += 1
            out = []
            while True:
                while pos < n and text[pos].isspace():
                    pos += 1
                if text[pos] == ")":
                    pos += 1
                    return out
                out.append(read())
        if c == '"':
            pos += 1
            buf = []
            while text[pos] != '"':
                if text[pos] == "\\":
                    pos += 1
                    buf.append({"n": "\n", "t": "\t"}.get(text[pos], text[pos]))
                else:
                    buf.append(text[pos])
                pos += 1
            pos += 1
            return "".join(buf)
        start = pos
        while pos < n and not text[pos].isspace() and text[pos] not in "()":
            pos += 1
        return Sym(text[start:pos])

    return read()


def dump(node, indent=0):
    if isinstance(node, list):
        simple = all(not isinstance(c, list) for c in node)
        if simple:
            return "(" + " ".join(dump(c) for c in node) + ")"
        pad = "\t" * (indent + 1)
        parts = [dump(node[0])]
        body = []
        for c in node[1:]:
            if isinstance(c, list):
                body.append("\n" + pad + dump(c, indent + 1))
            else:
                parts.append(dump(c))
        return "(" + " ".join(parts) + "".join(body) + "\n" + "\t" * indent + ")"
    if isinstance(node, Sym):
        return str(node)
    if isinstance(node, str):
        return '"' + node.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'
    if isinstance(node, float):
        return ("%.4f" % node).rstrip("0").rstrip(".")
    return str(node)


def find(node, name):
    return [c for c in node[1:] if isinstance(c, list) and c and c[0] == name]


def first(node, name):
    r = find(node, name)
    return r[0] if r else None
