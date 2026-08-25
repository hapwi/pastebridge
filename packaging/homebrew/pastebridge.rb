class Pastebridge < Formula
  desc "Copy on macOS, paste on Linux — encrypted local clipboard sync"
  homepage "https://github.com/hapwi/pastebridge"
  head "https://github.com/hapwi/pastebridge.git", branch: "main"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--root", prefix, "--path", "."
  end

  service do
    run [opt_bin/"pastebridge", "start"]
    keep_alive true
    log_path var/"log/pastebridge.log"
    error_log_path var/"log/pastebridge.err"
  end

  def caveats
    <<~EOS
      Pair this Mac with a Linux computer:
        pastebridge pair
      Then start the background service:
        brew services start pastebridge
    EOS
  end

  test do
    system "#{bin}/pastebridge", "--version"
  end
end
