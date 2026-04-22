class Pie < Formula
  desc "A minimal AI coding agent with sandboxed command execution"
  homepage "https://github.com/dineshdb/pie"
  url "https://github.com/dineshdb/pie/releases/download/v0.1.0/pie-0.1.0-aarch64-apple-darwin.tar.gz"
  sha256 "6cbba5777cb11c3f77cdfcaa87b68b6a8bf1ccf14a8e7761cfab89187e29468c"
  version "0.1.0"
  license "MIT"

  on_intel do
    url "https://github.com/dineshdb/pie/releases/download/v0.1.0/pie-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "91781cb0679c2a68b14d346e6eff4847eeeeca9f1a662fd7aa10923b95d4a2d4"
  end

  def install
    bin.install "pie"
    bin.install "p1e"
  end
end
