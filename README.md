# Merdia

Silly media display

## Development

You need Rust. Install via https://rustup.rs

### Ubuntu

Requirements:
```
apt install --no-install-recommends \
  build-essential \
  libgstreamer1.0-dev libgstreamer-plugins-* \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-rtp \
  gstreamer1.0-nice \
  libnice-dev libnice-10
```

### In a VM

When running in a VM, you can proxy the server to localhost like this:

```
socat TCP-LISTEN:3000,reuseaddr,fork,su=nobody TCP:192.168.64.2:3000
```
