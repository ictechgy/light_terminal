class Lterm < Formula
  desc "Lightweight tmux-compatible terminal session daemon"
  homepage "https://github.com/ictechgy/light_terminal"
  url "https://github.com/ictechgy/light_terminal/archive/refs/tags/v1.0.6.tar.gz"
  sha256 "145aa137683c10c7a8322e446b057c5aa4ca99742ec6bc34b8b0e1490601d6da"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ictechgy/light_terminal.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lterm 1.0.6", shell_output("#{bin}/lterm --version")
    assert_match "Light Terminal", shell_output("#{bin}/lterm --help")
  end
end
