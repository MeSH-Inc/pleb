# Homebrew tap formula template for pleb (https://github.com/MeSH-Inc/pleb).
# Copy into your Homebrew tap repository as Formula/pleb.rb when cutting a tagged release
# and replace VERSION / SHA256 with the values from the tagged source tarball.
class Pleb < Formula
  desc "Local, zero-telemetry web acquisition and search CLI for AI agents"
  homepage "https://github.com/MeSH-Inc/pleb"
  url "https://github.com/MeSH-Inc/pleb/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "cf02f4d5989895dc98118aefff292c339ec1a97ed8854b3390ae5ec75e59d6d0"
  license "MIT"
  head "https://github.com/MeSH-Inc/pleb.git", branch: "main"

  depends_on "rust" => :build

  def install
    # Build with default reqwest (rustls) TLS so Homebrew's offline sandbox
    # compiles without downloading external prebuilt Cronet archives.
    system "cargo", "install", *std_cargo_args
    (pkgshare/"scripts").install "scripts/install-cef-mac.sh",
                                 "scripts/install-cef-linux.sh",
                                 "scripts/download-cronet-macos.sh",
                                 "scripts/download-cronet-linux.sh",
                                 "scripts/install-skill.sh"
    (pkgshare/"cef").install Dir["cef/*"]
    (pkgshare/"skills").install "skills/pleb"
  end

  def caveats
    <<~EOS
      This formula builds pleb with the pure-Rust reqwest (rustls) transport.
      To register `pleb mcp`, bundled SKILL.md, and Claude Code WebFetch hook, run:
        #{bin}/pleb setup
      Or install only the bundled agent skill:
        #{bin}/pleb skill install
      To install the optional CEF v5-meta renderer, run:
        bash #{pkgshare}/scripts/install-cef-mac.sh --force
      For Chromium 154 Cronet TLS/HTTP2/QUIC fingerprinting, clone the repository and build with:
        ./scripts/download-cronet-macos.sh && cargo install --path . --features cronet --locked
    EOS
  end

  test do
    assert_match "pleb #{version}", shell_output("#{bin}/pleb --version")
    assert_match "name: pleb", shell_output("#{bin}/pleb skill print")
  end
end
