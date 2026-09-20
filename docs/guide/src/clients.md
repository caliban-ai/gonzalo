# Generating a client

Gonzalo ships **Rust deliverables only** ([ADR 0020](./adr/0020-rust-native-deliverables.md)).
There is no Python package, no npm package, and there will not be one — a
maintained SDK per language is a promise this project will not keep well.

What it ships instead is the **description of the surface**, so you can generate
the client you need and own it yourself. That is what
[ADR 0007](./adr/0007-dual-transport-daemon.md) means when it says the daemon
exists so non-Rust tools can share a store.

## The artifacts

Every release attaches both descriptions to its
[GitHub Release](https://github.com/caliban-ai/gonzalo/releases):

| file | describes | for |
|---|---|---|
| `gonzalo-vX.Y.Z.proto` | the gRPC surface | `protoc`, `buf`, and every gRPC toolchain |
| `gonzalo-openapi-vX.Y.Z.json` | the HTTP/JSON surface | OpenAPI generators, and anything that reads OpenAPI 3.1 |

Both also live in the repository — `crates/gonzalo-proto/proto/gonzalo.proto`
and [`docs/api/openapi.json`](https://github.com/caliban-ai/gonzalo/blob/main/docs/api/openapi.json) —
and the `.proto` ships inside the `gonzalo-proto` crate on crates.io.

**They cannot drift from what the daemon serves.** The route table, the router
and the OpenAPI document are checked against each other by the test suite, in
both directions: an operation added to the daemon without describing it fails
CI, and so does one described but not served. A description nothing checks is
worse than none, because it is believed.

```sh
gh release download v0.7.0 --repo caliban-ai/gonzalo \
  --pattern 'gonzalo-openapi-*.json' --pattern 'gonzalo-*.proto'
```

## A worked example: Python over HTTP/JSON

Generate a client with any OpenAPI generator. With
[`openapi-python-client`](https://github.com/openapi-generators/openapi-python-client):

```sh
pipx run openapi-python-client generate \
  --path gonzalo-openapi-v0.7.0.json \
  --meta none --output-path gonzalo_client
```

Then read and write records against a running daemon:

```python
import httpx

BASE = "http://localhost:8080"
TOKEN = "…"                      # omit when the daemon serves open
auth = {"Authorization": f"Bearer {TOKEN}"}

# Create: `expected` omitted means "this key must not exist yet", so this
# cannot silently overwrite someone else's record.
record = {
    "key": {"namespace": "memory", "collection": "topics", "id": "rust"},
    "kind": "Topic",
    "revision": {"counter": 0, "hash": ""},   # the store stamps the real one
    "body": {"Inline": list(b'{"slug":"rust","bullets":[]}')},
    "meta": {
        "author": {"id": "ada"},
        "origin_system": "example",
        "created": 0,
        "updated": 0,
        "labels": {},
    },
    "links": [],
}
put = httpx.put(
    f"{BASE}/v1/records/memory/topics/rust",
    json={"record": record, "expected": None},
    headers=auth,
).json()

if put["outcome"] == "conflict":
    # Not an error: someone got there first, and their record came back with it.
    current = put["conflict"]["current"]
    print("already at", current["revision"])
else:
    print("committed", put["revision"])

got = httpx.get(f"{BASE}/v1/records/memory/topics/rust", headers=auth).json()
print(bytes(got["body"]["Inline"]))
```

Three things about that exchange are worth keeping when you write your own
client, because they are the semantics rather than the syntax:

- **A conflict is data, not an error.** `PUT` answers `200` with
  `{"outcome": "conflict", …}` carrying the record the store actually holds
  ([ADR 0005](./adr/0005-optimistic-concurrency-and-conflict-surfacing.md)).
  Re-read, re-apply, retry — never retry blind.
- **`expected` is the whole concurrency story.** Omit it to create; pass the
  revision you read to update. A client that always omits it will find it can
  only ever create.
- **A delete is not a local erasure.** It writes a tombstone that replicates
  ([ADR 0021](./adr/0021-replicated-deletion-with-tombstones.md)), so a deleted
  key stays deleted when peers sync, and `GET /v1/records/...` returns `404`
  while `GET /v1/raw/records/...` still shows the tombstone.

## Over gRPC

Same story with `gonzalo.proto` and your language's gRPC plugin:

```sh
python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. gonzalo-v0.7.0.proto
```

The two transports carry the same types over one canonical schema, so which one
you generate from is a deployment choice rather than a semantic one.

## What not to do

Don't hand-write a client against these docs and keep it in sync by hand. Don't
vendor a copy of the schema that you forget to update. Regenerate from the
release artifact when you upgrade the daemon — that is the entire reason it is
published per release.
