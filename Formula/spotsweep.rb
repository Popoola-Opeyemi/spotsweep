class Spotsweep < Formula
  desc "Automatically discover development directories and manage Spotlight exclusions"
  homepage "https://github.com/Popoola-Opeyemi/spotsweep"
  url "https://github.com/Popoola-Opeyemi/spotsweep/releases/download/v0.3.0/spotsweep-macos-universal.zip"
  sha256 "c7b3c749166407e0d34d15546e4f4a26d488613003cd3320bbee3f994f1f7322"
  license "MIT"
  version "0.3.0"

  depends_on :macos

  def install
    bin.install "spotsweep"
    doc.install "README.md", "LICENSE", "PRIVATE_API.md"
  end

  service do
    run [opt_bin/"spotsweep", "daemon"]
    keep_alive true
    process_type :background
    nice 10
    log_path var/"log/spotsweep.log"
    error_log_path var/"log/spotsweep.log"
  end

  def caveats
    <<~EOS
      Upgrading from the old `spotlight-dev-exclusions` formula? Uninstall it
      first, then install SpotSweep:

        brew uninstall spotlight-dev-exclusions
        brew install spotsweep

      Existing data is migrated automatically to
      ~/Library/Application Support/SpotSweep.

      Automatic exclusion uses a private macOS Metadata.framework API and is
      gated by a per-build self-test.  Run it once before starting the service:

        spotsweep selftest

      Then start the background agent:

        brew services start spotsweep

      If the self-test fails or macOS changes the API, directories remain
      pending for the manual export workflow:

        spotsweep export
        spotsweep mark --all
        spotsweep verify --confirm

      See: #{doc}/PRIVATE_API.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/spotsweep --version")
  end
end
