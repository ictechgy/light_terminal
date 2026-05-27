class Lterm < Formula
  desc "Lightweight tmux-compatible terminal session daemon"
  homepage "https://github.com/ictechgy/light_terminal"
  url "https://github.com/ictechgy/light_terminal/archive/refs/tags/v1.0.7.tar.gz"
  sha256 "859f81205412e19e9502770a9c0f8514164ec3d511a8150f0a2af9608f7e867d"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ictechgy/light_terminal.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lterm 1.0.7", shell_output("#{bin}/lterm --version")
    assert_match "Light Terminal", shell_output("#{bin}/lterm --help")
  end
end
