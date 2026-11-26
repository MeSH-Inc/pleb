# Homebrew tap formula template for pleb (https://github.com/MeSH-Inc/pleb).
# Copy into your Homebrew tap repository as Formula/pleb.rb when cutting a tagged release
# and replace VERSION / SHA256 with the values from the tagged source tarball.
class Pleb < Formula
  desc "Local, zero-telemetry web acquisition and search CLI for AI agents"
  homepage "https://github.com/MeSH-Inc/pleb"
  url "https://github.com/MeSH-Inc/pleb/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "f99e271ed7a49ee2fe66f891abd7d55dc50dd4d7005d05145a03d02ffaa79e79"
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
    (pkgshare/"skills").install "skills/pleb"
  end

  def caveats
    <<~EOS
      This formula builds pleb with the pure-Rust reqwest (rustls) transport.
      For Chromium 154 Cronet TLS/HTTP2/QUIC fingerprinting or the optional
      local CEF v5-meta renderer, clone the repository and build with:
        ./scripts/download-cronet-macos.sh && cargo install --path . --features cronet --locked
        bash scripts/install-cef-mac.sh --force
    EOS
  end

  test do
    assert_match "pleb #{version}", shell_output("#{bin}/pleb --version")
    assert_match "name: pleb", shell_output("#{bin}/pleb skill print")
  end
end
