class Lterm < Formula
  desc "Lightweight tmux-compatible terminal session daemon"
  homepage "https://github.com/ictechgy/light_terminal"
  url "https://github.com/ictechgy/light_terminal/archive/refs/tags/v1.0.11.tar.gz"
  sha256 "ceba1755f81250475c6d2ef3418f2bf34876771b2963230346f6b988081f058e"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ictechgy/light_terminal.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lterm 1.0.11", shell_output("#{bin}/lterm --version")
    assert_match "Light Terminal", shell_output("#{bin}/lterm --help")
  end
end
