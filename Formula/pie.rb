class Pie < Formula
  desc "A minimal AI coding agent with sandboxed command execution"
  homepage "https://github.com/dineshdb/pie"
  url "https://github.com/dineshdb/pie/releases/download/v0.2.0/pie-0.2.0-aarch64-apple-darwin.tar.gz"
  sha256 "b305b71b65c2b5178526b4224b041d7edfa220f92de27c19e68664bac715c3fa"
  version "0.2.0"
  license "MIT"

  on_intel do
    url "https://github.com/dineshdb/pie/releases/download/v0.2.0/pie-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "367a1340399da358f722ee652c0df68ace249314606e5e2f6d0bc63595b7f02e"
  end

  def install
    bin.install "pie"
    bin.install "p1e"
  end
end
