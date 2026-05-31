class Lterm < Formula
  desc "Lightweight tmux-compatible terminal session daemon"
  homepage "https://github.com/ictechgy/light_terminal"
  url "https://github.com/ictechgy/light_terminal/archive/refs/tags/v1.0.13.tar.gz"
  sha256 "57f4ee654477f1eaa0cf4a4d26ad8b481825469111e4bb596e282e8df02f9c3c"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ictechgy/light_terminal.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lterm 1.0.13", shell_output("#{bin}/lterm --version")
    assert_match "Light Terminal", shell_output("#{bin}/lterm --help")
  end
end
