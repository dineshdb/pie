class Pie < Formula
  desc "A minimal AI coding agent with sandboxed command execution"
  homepage "https://github.com/dineshdb/pie"
  url "https://github.com/dineshdb/pie/releases/download/v0.1.0/pie-0.1.0-aarch64-apple-darwin.tar.gz"
  sha256 "placeholder"
  version "0.1.0"
  license "MIT"

  on_intel do
    url "https://github.com/dineshdb/pie/releases/download/v0.1.0/pie-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "placeholder"
  end

  def install
    bin.install "pie"
    bin.install "p1e"
  end
end
