# APT repository

Prebuilt `waylandwebstream` packages for **Debian 13 (trixie), amd64** are
published at:

    https://derlaft.github.io/waylandwebstream-rs

The repository is signed; `apt` verifies every update against the project's
public key. Packages are built for trixie's library ABI (FFmpeg 7.1), so trixie
is the supported target.

## Install (end users)

```sh
# 1. Trust the repository signing key.
sudo install -d -m 0755 /usr/share/keyrings
curl -fsSL https://derlaft.github.io/waylandwebstream-rs/KEY.gpg \
  | sudo tee /usr/share/keyrings/waylandwebstream-archive-keyring.gpg > /dev/null

# 2. Add the source.
sudo tee /etc/apt/sources.list.d/waylandwebstream.sources > /dev/null <<'EOF'
Types: deb
URIs: https://derlaft.github.io/waylandwebstream-rs
Suites: trixie
Components: main
Architectures: amd64
Signed-By: /usr/share/keyrings/waylandwebstream-archive-keyring.gpg
EOF

# 3. Install.
sudo apt update
sudo apt install waylandwebstream
```

Then enable it as a per-user service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now waylandwebstream
# open http://127.0.0.1:8080
```

It binds loopback only and has no authentication of its own — put an
authenticating reverse proxy in front before exposing it. See the
[README](../README.md) and `systemctl --user edit waylandwebstream` to run a
session app/compositor inside the stream.

## Repository layout

A standard flat APT archive served as static files from the `gh-pages` branch:

```
/
├── KEY.gpg                       # public signing key (armored)
├── dists/
│   └── trixie/
│       ├── InRelease             # signed (inline)
│       ├── Release
│       ├── Release.gpg           # signed (detached)
│       └── main/binary-amd64/
│           ├── Packages[.gz]
│           └── Release
└── pool/
    └── main/w/waylandwebstream/
        └── waylandwebstream_<version>_amd64.deb
```

## Trust model (maintainer)

Releases are cut by a manual `workflow_dispatch` (see
`.github/workflows/release.yml`); the package is built, the archive assembled,
and the `Release` file signed entirely in CI. The "final say" is that **only the
maintainer can trigger the release workflow** — signing never happens
automatically.

The GPG **signing subkey lives in GitHub Secrets without a passphrase**. This is
a deliberate simplicity tradeoff (a deep GitHub/Secrets compromise could sign),
mitigated by shrinking the key's blast radius:

- The **primary key is certify-only and kept offline** (air-gapped); only a
  short-expiry **signing subkey** is in Secrets.
- If the subkey leaks, **revoke and reissue a new subkey under the same primary**
  — the public key users trust (`KEY.gpg`) never changes, so installed clients
  keep working without re-trusting anything.
- The signing secret is scoped to a protected GitHub Environment; only the
  release job can read it. The release runs on `workflow_dispatch` only.
