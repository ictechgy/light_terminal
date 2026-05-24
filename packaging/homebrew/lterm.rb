class Lterm < Formula
  desc "Lightweight tmux-compatible terminal session daemon"
  homepage "https://github.com/ictechgy/light_terminal"
  url "https://github.com/ictechgy/light_terminal/archive/refs/tags/v1.0.5.tar.gz"
  sha256 "f0c4dadaa37766229971e52eae5a2bcccfe061dd746bd8a66e96d84c28559dd9"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/ictechgy/light_terminal.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "lterm 1.0.5", shell_output("#{bin}/lterm --version")
    assert_match "Light Terminal", shell_output("#{bin}/lterm --help")
  end
end
