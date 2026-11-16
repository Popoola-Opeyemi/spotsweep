class SpotlightDevExclusions < Formula
  desc "Discover dev build directories and exclude them from Spotlight"
  homepage "https://github.com/Popoola-Opeyemi/spotlight-dev-exclusions"
  url "https://github.com/Popoola-Opeyemi/spotlight-dev-exclusions/releases/download/v0.2.0/spotlight-dev-macos-universal.zip"
  sha256 "61359daf6ad3f64fabe324f9ad75a1101556156ecfca50687b3439bc7d8c56fa"
  license "MIT"
  version "0.2.0"

  depends_on :macos

  def install
    bin.install "spotlight-dev"
    doc.install "README.md", "LICENSE", "PRIVATE_API.md"
  end

  service do
    run [opt_bin/"spotlight-dev", "daemon"]
    keep_alive true
    process_type :background
    nice 10
    log_path var/"log/spotlight-dev-exclusions.log"
    error_log_path var/"log/spotlight-dev-exclusions.log"
  end

  def caveats
    <<~EOS
      Automatic exclusion uses a private macOS Metadata.framework API and is
      gated by a per-build self-test.  Run it once before starting the service:

        spotlight-dev selftest

      Then start the background agent:

        brew services start spotlight-dev-exclusions

      If the self-test fails or macOS changes the API, directories remain
      pending for the manual export workflow:

        spotlight-dev export
        spotlight-dev mark --all
        spotlight-dev verify --confirm

      See: #{doc}/PRIVATE_API.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/spotlight-dev --version")
  end
end
