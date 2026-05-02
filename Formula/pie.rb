class Pie < Formula
  desc "A minimal AI coding agent with sandboxed command execution"
  homepage "https://github.com/dineshdb/pie"
  url "https://github.com/dineshdb/pie/releases/download/v0.3.0/pie-0.3.0-aarch64-apple-darwin.tar.gz"
  sha256 "9e76040178a57f5e35c00b71b75d430533fe72dc8321ecb0f17965b5861edb79"
  version "0.3.0"
  license "MIT"

  on_intel do
    url "https://github.com/dineshdb/pie/releases/download/v0.3.0/pie-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "cc9df14ae1ea98aac1bd4537e2bae283c0b1f58ad53a9eb5548686454d22936c"
  end

  def install
    bin.install "pie"
    bin.install "p1e"
  end
end
