# Raw Rust Web Server

This program sends the files of your website (HTML pages, CSS, JavaScript, and images) from a folder on your server.

**Benchmark:** 715,813 requests/sec for `cache-all` on the home page with Brotli at 256 connections. See [Benchmarks](#benchmarks).

This server is meant to run on port 80 behind Cloudflare. Cloudflare connects to your server on port 80, and Cloudflare handles HTTPS for visitors.

## Contents

- [Which version should I use?](#which-version-should-i-use)
- [Quick start (5 minutes)](#quick-start-5-minutes)
- [Using Cloudflare](#using-cloudflare)
- [Keep it running](#keep-it-running)
- [Settings](#settings)
- [Other systems](#other-systems)
- [Troubleshooting / FAQ](#troubleshooting--faq)
- [Benchmarks](#benchmarks)
- [License](#license)

## Which version should I use?

There are eight folders. Each one is a complete copy of the same server. The only difference is the settings block at the top of that folder's `main.rs` (how long browsers may keep files, and whether text is compressed).

**Use `cache-all` for most sites.** Browsers are told to keep every file for a year, and text is compressed, so pages load quickly. The steps below build that folder.

Compression here means [Brotli](https://github.com/google/brotli). The server shrinks text such as HTML, CSS, JavaScript, SVG, and JSON before sending it. Most pictures, plus video, audio, and zip files, are sent unchanged. Folder names with `no-compression` send every file unchanged and use less CPU while starting.

| Folder | In one sentence |
|---|---|
| `cache-all` | **Recommended.** Browsers keep every file for a year, and text is compressed. |
| `cache-assets-only` | Pages are checked on every visit, while pictures, CSS, and JavaScript are kept for a year, with text compressed. |
| `cache-1-minute` | Pages are kept for one minute, pictures, CSS, and JavaScript for a year, with text compressed. |
| `no-cache` | Every file is sent fresh, with no browser cache and no compression, which is handy while you are testing. |
| `no-cache+compression` | Every file is sent fresh, and text is compressed so downloads are smaller. |
| `cache-all-no-compression` | Browsers keep every file for a year, and the server does no compressing of its own. |
| `cache-assets-only-no-compression` | Pages stay fresh, pictures, CSS, and JavaScript are kept for a year, and nothing is compressed by the server. |
| `cache-1-minute-no-compression` | Pages are kept for one minute, pictures, CSS, and JavaScript for a year, with no compression. |

To use a folder other than `cache-all`, use that folder's name everywhere the steps say `cache-all`.

A few details that apply to every folder:

- The six `cache-*` folders send an **ETag** (a fingerprint of the file). When the browser already has that exact file, the server answers `304 Not Modified` and skips sending the file again. The two `no-cache` folders skip ETags on purpose.
- On the compression versions, the site answers as soon as the files are loaded. A stronger Brotli pass finishes in the background shortly after.
- `/about` and `/about/` both open `about/index.html` when that file exists.
- Video and audio can start in the middle of the file (byte ranges). Safari on iPhone, iPad, and Mac needs this to play media at all.
- After you change a file on disk, restart the server so it reads the new copy. With `cache-all`, browsers keep pages for a year. When you change a page, picture, or stylesheet, give it a new name or add a version to the link, such as `style.css?v=2`. If Cloudflare is also caching that file, purge its cache too (see [Using Cloudflare](#using-cloudflare)).

## Quick start (5 minutes)

These steps are for a fresh **Ubuntu** or **Debian** server. You end with `cache-all` listening on port **80**, which is the only port this server should use.

Port 80 is reserved for the administrator account (**root**). If you are not root, start the server with `sudo`, or grant the program permission with `setcap`, as in the start step. Both ways still use port 80. If a command says `sudo: command not found`, you are already root, so run the `apt` lines without `sudo`.

### 1. Install the tools

This installs a C compiler (Rust needs it to finish the build), `git`, `screen`, and Rust itself. Rust 1.85 or newer is required. `cargo --version` prints the Rust build tool's version when the install worked.

```bash
sudo apt update
sudo apt install -y build-essential curl git screen
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo --version
```

If a later command says `cargo: command not found`, run `. "$HOME/.cargo/env"` again. New terminals pick it up on their own.

### 2. Download the code

`git clone` copies this repository into your home folder.

```bash
cd "$HOME"
git clone https://github.com/dumptcp/rawrustwebserver
```

### 3. Open the cache-all folder

`cache-all` is the version most people should run.

```bash
cd "$HOME/rawrustwebserver/cache-all"
```

### 4. Build the server

The first build downloads libraries and can take a few minutes. `--locked` uses the library versions recorded in `Cargo.lock`, which are the ones this server was tested with. When it finishes, the program is `target/release/rawstatic`.

```bash
cd "$HOME/rawrustwebserver/cache-all"
cargo build --release --locked
```

### 5. Add your website files

The server reads files from a folder you choose. The setting is called `ROOT`. This step creates `$HOME/www` and a one-page test site. Replace `index.html` with your real site when you are ready (upload with `scp`, or with an SFTP app such as WinSCP, FileZilla, or MobaXterm). The home page must be named `index.html` and must sit directly inside that folder.

```bash
mkdir -p "$HOME/www"
cat > "$HOME/www/index.html" << 'EOF'
<!DOCTYPE html>
<html>
<head><title>It works</title></head>
<body><h1>It works</h1></body>
</html>
EOF
```

Names that start with `.` are hidden, except a top-level `.well-known` folder (used by Let's Encrypt and similar tools). Symbolic links are skipped.

### 6. Start the server

Stay in `cache-all`. This listens on port 80 and serves `$HOME/www`. Leave the terminal open. When it is ready it prints `listening on ...`. It does not return to a prompt. That means it is running.

`sudo` runs the program as root so it can use port 80:

```bash
cd "$HOME/rawrustwebserver/cache-all"
sudo ROOT="$HOME/www" ./target/release/rawstatic
```

If you are not root and would rather start it as yourself, grant this copy of the program permission to use port 80, then start it without `sudo`. Rebuilding replaces the file, so run `setcap` again after every build:

```bash
cd "$HOME/rawrustwebserver/cache-all"
sudo setcap cap_net_bind_service=+ep ./target/release/rawstatic
ROOT="$HOME/www" ./target/release/rawstatic
```

If you are already root, leave `sudo` off. If you also leave `ROOT` unset, the folder is `/root/www`. Put `index.html` there.

### 7. Check that it works

Open a **second** SSH session. The first one is busy running the server. `curl` asks the server for the page headers. You want a line that says `HTTP/1.1 200 OK`. `hostname -I` prints this machine's addresses. In a browser on your own computer, open `http://THAT_ADDRESS/`.

```bash
curl -I http://127.0.0.1/
hostname -I
```

Press `Ctrl+C` in the server's terminal to stop it. It stops taking new visitors, lets downloads already in progress finish (for up to 30 seconds), then exits.

Leave the server running and set up Cloudflare next. That is how visitors reach the site.

## Using Cloudflare

Do this as part of setup, once the server is running on port 80. Visitors use your domain through Cloudflare. Cloudflare connects to this server on port 80 and handles HTTPS for them. This server speaks plain HTTP on port 80.

In the Cloudflare dashboard:

1. **DNS.** Point your domain at this server's IP address. Leave the record **Proxied** (orange cloud).
2. **SSL/TLS → Overview.** Set the mode to **Flexible**. Flexible matches a server that speaks plain HTTP on port 80. **Full** and **Full (strict)** make Cloudflare try HTTPS to your server, and visitors then get an error page.
3. Turn on **Always Use HTTPS** (SSL/TLS → Edge Certificates) so `http://` visitors are sent to `https://`.
4. Turn on **Smart Tiered Cache** (Caching → Tiered Cache). Cloudflare's data centers then share files with each other instead of each one asking your server.

Cloudflare caches images, CSS, JavaScript, fonts, video, and audio by default. It does not cache HTML or JSON (it decides by the file extension). With `cache-assets-only`, that matches the server: pages are checked with your server, and other files are answered from Cloudflare. To have Cloudflare cache HTML as well (`cache-all` or `cache-1-minute`), add a **Cache Rule** (Caching → Cache Rules): **Eligible for cache**, Edge TTL **Use cache-control header if present**, Browser TTL **Respect origin**. Those settings make Cloudflare use the times this server sends. For `.json` files, add a rule whose filter is **File extension is in** `json`, with **Eligible for cache** and Edge TTL **Use cache-control header if present**.

The folders that compress text send `Cache-Control: no-transform` on every file. That tells Cloudflare to pass the file on exactly as the server sent it. Cloudflare then keeps the file's ETag (so `304` replies still work), does not compress it a second time, and does not insert its own scripts into your pages. Cloudflare's **JavaScript Detections** (Security → Bots) and the automatic **Web Analytics** script are therefore not added on those folders. The folders that do not compress text add `no-transform` only to files that are already compressed (images except SVG and ICO, video, audio, WOFF/WOFF2 fonts, PDFs, and zip files). HTML, CSS, JavaScript, SVG, ICO, and TTF/OTF fonts stay open so Cloudflare can compress them. On those folders, JavaScript Detections removes the ETag from pages. Turn JavaScript Detections off, and turn off **Bot Fight Mode** (it turns JavaScript Detections on), when you want those pages answered with `304`.

### The firewall file

`cloudflare.conf` (in the repository root, next to the version folders) is a Linux **nftables** firewall. After you load it, only [Cloudflare's addresses](https://www.cloudflare.com/ips/) can open port 80. SSH still works. Other visitors are dropped. It replaces the `inet webfw` table and leaves every other firewall table alone, including Docker's.

Load it only after the domain is **Proxied** through Cloudflare. Otherwise nobody can open the site, because the file drops anyone who is not Cloudflare.

> **Warning:** set `SSH_PORT` to the port you actually use to log in (22 unless you changed it). Leave `WEB_PORT` at 80. A wrong `SSH_PORT` locks you out of SSH. Keep your current SSH window open, and test a second login before you close it. If the second login fails, fix the file from the window that is still open, or use your hosting provider's web console.

Open `cloudflare.conf` and check the two lines near the top. Change `SSH_PORT` only when you log in on a different port. Leave `WEB_PORT` at 80:

```
define SSH_PORT = 22
define WEB_PORT = 80
```

Install the firewall tool if the server does not have it yet, then load the file:

```bash
sudo apt install -y nftables
sudo nft -f "$HOME/rawrustwebserver/cloudflare.conf"
```

Check that the table is there:

```bash
sudo nft list table inet webfw
```

The rules are gone after a reboot unless you save them. Copying this file to `/etc/nftables.conf` makes it the firewall that loads on boot. If that file already has rules you need, merge them in before you copy, or skip these two commands and load `cloudflare.conf` by hand after each reboot.

```bash
sudo cp "$HOME/rawrustwebserver/cloudflare.conf" /etc/nftables.conf
sudo systemctl enable nftables
```

Loading the file again replaces only the `inet webfw` table, so it is safe to repeat after you set `SSH_PORT`. Remove just this firewall with:

```bash
sudo nft delete table inet webfw
```

`sudo nft flush ruleset` clears every firewall table on the machine, including tables this file did not create. Delete `inet webfw` when you only want this file gone.

Cloudflare's address list changes rarely. When it does, update both lists in `cloudflare.conf` from [cloudflare.com/ips](https://www.cloudflare.com/ips/) and load the file again.

## Keep it running

Closing SSH stops a program you started in that window. Pick one of the two options below.

### screen (simplest)

`screen` keeps the program running after you disconnect. Start a session named `website`:

```bash
cd "$HOME/rawrustwebserver/cache-all"
screen -S website
```

You are now inside that session. Start the server on port 80 the same way as before:

```bash
sudo ROOT="$HOME/www" ./target/release/rawstatic
```

Leave it running in the background: press `Ctrl+A`, release both keys, then press `D`. You are back at a normal prompt, and the server is still going.

Come back to it later (to press `Ctrl+C`, or to read what it printed):

```bash
screen -rx website
```

### systemd (starts when the machine boots)

systemd is Linux's service manager. The file below starts the site on boot and restarts it if it crashes. It is optional. `screen` is enough for most people.

Replace every `youruser` with your account name. On Ubuntu the group name is usually the same as the user name. That account must be allowed to read the program and the site folder. `AmbientCapabilities` lets that user listen on port 80.

Create `/etc/systemd/system/rawstatic.service`:

```ini
[Unit]
Description=rawstatic website
After=network.target

[Service]
User=youruser
Group=youruser
WorkingDirectory=/home/youruser/rawrustwebserver/cache-all
Environment=ROOT=/home/youruser/www
AmbientCapabilities=CAP_NET_BIND_SERVICE
ExecStart=/home/youruser/rawrustwebserver/cache-all/target/release/rawstatic
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Load it and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rawstatic
sudo systemctl status rawstatic
```

Stop it, start it, and follow the log:

```bash
sudo systemctl stop rawstatic
sudo systemctl start rawstatic
sudo journalctl -u rawstatic -f
```

## Settings

Settings are environment variables: words you put on the same line, in front of the program. Anything you leave off uses the default. A bad value prints `bad NAME: ...` and the program exits.

```bash
sudo ROOT="$HOME/www" CACHE_MEMORY=512M ./target/release/rawstatic
```

Usable memory means the smaller of the machine's RAM and any container memory limit. If the RAM size cannot be read, the server uses 1 GiB. On startup it prints that memory, the connection limit, the header budget, and (once files are loaded) how many files are cached in RAM versus read from disk.

| Name | Default | What it does | Example |
|---|---|---|---|
| `ROOT` | `/root/www` | The folder your site files are in. Set this to the folder that contains `index.html`. The default is root's home, which fits after you log in as root. | `ROOT="$HOME/www"` |
| `KEEP_ALIVE` | `120` | How many seconds an idle connection stays open. The smallest value used is 1. The default is 120 seconds so idle sockets are not held for Cloudflare's 900 second origin keep-alive. | `KEEP_ALIVE=60` |
| `CACHE_MEMORY` | A quarter of usable memory | How much RAM file bodies and their compressed copies may use together. A plain byte count, or a 1024-based size: `512M`, `2G`, `512MB`, `1GiB`. Files that do not fit are read from disk when someone asks for them, and those copies are not Brotli-compressed. A site larger than RAM still starts. | `CACHE_MEMORY=512M` |
| `MAX_IN_MEMORY` | `2097152` (2 MB) | One file larger than this is always read from disk, even when `CACHE_MEMORY` has room. A plain number of bytes. `2M` is not accepted here. | `MAX_IN_MEMORY=10485760` |
| `MAX_CONNECTIONS` | none | How many connections are served at once. Unset or `0` means no cap: the only limits are the operating system's open files and memory. On Linux, macOS, and FreeBSD the server raises the open-file soft limit to the hard limit at startup. If `accept` runs out of descriptors, it waits briefly and keeps accepting. A number above zero is a cap. Past it, the connection is still accepted, answered with `503 Service Unavailable`, `Retry-After: 1`, and an empty body, then closed. | `MAX_CONNECTIONS=10000` |
| `HEAD_MEMORY` | A sixteenth of usable memory | Extra RAM allowed for request headers that have not finished arriving, added up across every connection. Past this, that connection gets `431` (headers too large) and is closed. One header still cannot be longer than 128 KB. A plain number of bytes. | `HEAD_MEMORY=67108864` |

Cache times (one year, one minute, or none) are chosen by the folder you built. They are not environment variables. To change them, build a different folder, or edit the settings block at the top of `main.rs` and build again.

A few limits are fixed in the program. Before the first request, 10 seconds of silence closes the connection. After a reply, an idle connection stays open for `KEEP_ALIVE` seconds. A header that has started arriving has 10 seconds to finish. A connection that stops reading is dropped after 30 seconds with no send progress. A keep-alive connection is also closed after 100,000 responses.

## Other systems

The same `cache-all` build works on macOS, FreeBSD, and Windows. `cloudflare.conf` is Linux only.

**macOS.** Install the Xcode command line tools, then Rust:

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Clone the repository, `cd` into `cache-all`, and run `cargo build --release --locked`, as on Linux. Port 80 needs `sudo`.

**FreeBSD.** As root, install `curl`, `git`, and a C compiler from packages, then Rust the same way:

```bash
pkg install curl git gcc
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Build and run as on Linux. Port 80 needs root.

**Windows.** Install Rust from [https://rustup.rs](https://rustup.rs). In the `cache-all` folder run `cargo build --release --locked`. The program is `target\release\rawstatic.exe`. Windows listens with one socket for the whole machine. Linux, macOS, and FreeBSD use one listener per CPU core. In Command Prompt, from the `cache-all` folder:

```bat
set ROOT=C:\www
target\release\rawstatic.exe
```

Open Command Prompt as Administrator so the program can listen on port 80. Put `index.html` in `C:\www` first. There is no Unix open-file limit to raise on Windows. Without `MAX_CONNECTIONS`, there is no connection cap.

## Troubleshooting / FAQ

### Permission denied on port 80

The server prints `listen on :80: permission denied`. Port 80 needs the administrator account. Start it with `sudo`:

```bash
cd "$HOME/rawrustwebserver/cache-all"
sudo ROOT="$HOME/www" ./target/release/rawstatic
```

Or grant this copy of the program permission to use port 80, then start it as yourself. Run `setcap` again after every rebuild:

```bash
cd "$HOME/rawrustwebserver/cache-all"
sudo setcap cap_net_bind_service=+ep ./target/release/rawstatic
```

### Port already in use

The server prints `listen on :80: port 80 is already in use`. Another program already has port 80. That is often Apache (`apache2`), nginx, or an older copy of this server. Stop that program. Only one program can listen on port 80, so visitors are never split between an old and a new copy.

See who has port 80, then stop the usual web servers:

```bash
sudo ss -ltnp 'sport = :80'
sudo systemctl stop apache2
sudo systemctl stop nginx
```

If the list shows an older copy of this server, stop it (`Ctrl+C`, or `sudo systemctl stop rawstatic`) and start the new one.

### 404, or "no files found"

The server only serves files inside `ROOT`. If that folder is missing or empty, it prints `no files found under ...` and exits. The default `ROOT` is `/root/www`, so a start command that forgets `ROOT` looks in root's home, not in your home.

If the server stays running and the browser shows **404**, the requested file is not in that folder. The home page has to be `index.html` directly inside `ROOT`, not in a subfolder. The account that starts the server has to be allowed to read the folder. Names that start with `.` are hidden, except a top-level `.well-known` folder. Symbolic links are skipped.

To show your own missing-page, put a `404.html` in the top of the site folder. Inside it, start links to CSS and images with `/` (for example `/style.css`), because that page is shown at many different addresses.

### Out of memory, or CACHE_MEMORY

The server keeps files in RAM only until it reaches `CACHE_MEMORY` (a quarter of usable memory unless you set it), and only when the file is no bigger than `MAX_IN_MEMORY` (2 MB unless you set it). Everything past that is read from disk. The startup lines report the budget and the split.

If the machine is small and the process is killed while starting, give it a smaller budget and start again:

```bash
cd "$HOME/rawrustwebserver/cache-all"
sudo CACHE_MEMORY=128M ROOT="$HOME/www" ./target/release/rawstatic
```

`CACHE_MEMORY` accepts sizes such as `128M` and `1G`. `MAX_IN_MEMORY` and `HEAD_MEMORY` are plain byte counts.

### A browser cannot connect, but curl on the server works

`curl` to `127.0.0.1` talks to the program on this machine, so it still works when `cloudflare.conf` is loaded. A line that says `HTTP/1.1 200 OK` means the server is running:

```bash
curl -I http://127.0.0.1/
```

If you loaded `cloudflare.conf`, only Cloudflare can reach port 80. That is what the file is for. A browser pointed at the server's IP address is not Cloudflare, so it will not connect. Visitors open the site by its domain after the DNS record is **Proxied** (see [Using Cloudflare](#using-cloudflare)).

If you loaded the file before the domain was proxied, nobody outside Cloudflare can open the site. Remove the `inet webfw` table, then load `cloudflare.conf` again once the record is **Proxied**:

```bash
sudo nft delete table inet webfw
```

### How to stop

In the server's terminal, press `Ctrl+C`. A `kill` of the process does the same. The server stops accepting new visitors, waits up to 30 seconds for downloads already in progress, then exits. Inside `screen`, run `screen -rx website` first. With systemd:

```bash
sudo systemctl stop rawstatic
```

### How to update

Website files: replace the files in your `ROOT` folder, then stop the server and start it again. You do not need to build again for a file change.

The program itself: pull the new code, build again, then stop the old process and start the new one.

```bash
cd "$HOME/rawrustwebserver"
git pull
cd cache-all
cargo build --release --locked
```

Build again only when `main.rs` or `Cargo.toml` changed. A clean rebuild starts from:

```bash
cargo clean
```

### The page looks old after I edit it

Restart the server after any file change. `cache-all` tells browsers to keep pages, pictures, CSS, and JavaScript for a year. Give a changed file a new name, or change the link to `style.css?v=2`, so browsers fetch it again. If you added a Cloudflare cache rule for HTML, also purge Cloudflare (Caching → Configuration → Purge Everything).

`cache-assets-only` is the folder to use when pages change often and you want every visit to check for a new page.

### It will not run PHP, Python, or a form POST

This server sends files stored on disk. It does not run PHP, Python, or any other program. It answers `GET` and `HEAD`. A form or script that sends `POST`, `PUT`, or `DELETE` gets `405 Method Not Allowed`.

### Fonts, scripts, or embeds from another site are blocked

Browsers are told to load things only from your own site. That header is `Content-Security-Policy`. To allow another site, open `main.rs`, find `CONTENT_SECURITY_POLICY_VALUE`, and add the site to the matching line. All eight folders ship the same policy.

| You want | Line to change | What to add |
|---|---|---|
| Google Fonts | `style-src` and `font-src` | `https://fonts.googleapis.com` on `style-src`, `https://fonts.gstatic.com` on `font-src` |
| A script from a CDN | `script-src` | The CDN, for example `https://cdn.jsdelivr.net` |
| Images or video hosted elsewhere | `img-src` or `media-src` | The site they come from, for example `https://images.example.com` |
| A YouTube embed | Add a directive | `frame-src https://www.youtube.com` |
| Analytics or an API | `connect-src` (and `script-src` for its script) | The site, for example `https://api.example.com` |

Google Fonts looks like this:

```
    style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
    font-src 'self' https://fonts.gstatic.com; \
```

Build again with `cargo build --release --locked` and restart.

Every response also sends `Strict-Transport-Security` for one year, including subdomains (`includeSubDomains`). Each subdomain has to work over HTTPS. With Cloudflare proxying them, they do. Otherwise, remove `; includeSubDomains` from `strict-transport-security` in `main.rs` and build again.

## Benchmarks

On Tuesday Nov 24, 2026 (4:16–4:22 AM PT), the phoenixNAP dedicated server in Chicago (Intel Xeon E-2388G, 8 cores / 16 threads, 62 GiB RAM, Ubuntu 26.04.1, kernel 7.0, tuned sysctl and BBR) built each server's `cache-all` from main and ran it alone on 127.0.0.1 with no logging, serving the same 7-file site. The keep-alive default is now 120s. Commits: rawrustwebserver `9832e7b`, rustwebserver `4ec454c`, golangwebserver `d63bff2`. The server was pinned with `taskset` to CPUs 0–3 and 8–11 (4 cores / 8 threads) and the load tool, oha 1.17.0, to CPUs 4–7 and 12–15, so the two never competed for a core. After a 3s warmup, oha ran three 10-second tests at 256 connections (Rust 1.100.0, Go 1.27.4), and the median is reported. The 4096-connection test is one 10-second run. "Home + Brotli" sends `Accept-Encoding: br` (3,128-byte response). "Home plain" sends `Accept-Encoding: identity` (13,976-byte response); the Nov 22 run left the header out, but oha then sends `gzip, compress, deflate, br` by default, so that row was really Brotli too.

Requests/sec (RPS) is how many requests came back each second (higher is better); typical latency is p50, and slow latency is p99. Transfer is response bytes per second, and memory is peak RSS.

This server is rawrust. [rustwebserver](https://github.com/dumptcp/rustwebserver) is Actix, and [golangwebserver](https://github.com/dumptcp/golangwebserver) is Fiber.

### Quick winner

Home page with Brotli, 256 connections.

| Server | Requests/sec | Avg latency | Typical latency | Slow latency (p99) | Transfer | Memory |
|---|---:|---:|---:|---:|---:|---:|
| **rawrust** | **715,813** | **0.356 ms** | **0.312 ms** | **0.498 ms** | **2.24 GB/s** | **8.4 MB** |
| [actix](https://github.com/dumptcp/rustwebserver) | 599,974 | 0.425 ms | 0.402 ms | 0.763 ms | 1.88 GB/s | 11.3 MB |
| [golang](https://github.com/dumptcp/golangwebserver) | 508,184 | 0.502 ms | 0.399 ms | 1.788 ms | 1.59 GB/s | 17.8 MB |

### All numbers

Requests per second. Bold is the fastest in that row.

| What | Connections | rawrust | actix | golang |
|---|---:|---:|---:|---:|
| Home + Brotli | 256 | **715,813** | 599,974 | 508,184 |
| Home plain | 256 | **472,350** | 459,861 | 429,407 |
| Home + Brotli | 4096 | **386,559** | 366,650 | 371,058 |

Home plain, 256 connections, typical latency / slow latency (p99) and transfer: rawrust 0.494 ms / 0.760 ms, **6.60 GB/s**; actix 0.563 ms / **0.723 ms**, 6.43 GB/s; golang **0.468 ms** / 2.177 ms, 6.00 GB/s.

### 4096 connections

Home page with Brotli, one 10-second run.

| Server | Requests/sec | Avg latency | Typical latency | Slow latency (p99) | Transfer | Memory |
|---|---:|---:|---:|---:|---:|---:|
| rawrust | **386,559** | **10.57 ms** | **10.63 ms** | **11.97 ms** | **1.21 GB/s** | **24.1 MB** |
| [actix](https://github.com/dumptcp/rustwebserver) | 366,650 | 11.15 ms | 11.11 ms | 14.13 ms | 1.15 GB/s | 78.6 MB |
| [golang](https://github.com/dumptcp/golangwebserver) | 371,058 | 11.01 ms | 10.84 ms | 20.13 ms | 1.16 GB/s | 44.0 MB |

---

## License

MIT — see [LICENSE](LICENSE).
