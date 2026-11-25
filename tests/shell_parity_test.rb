require "fileutils"
require "io/console"
require "minitest/autorun"
require "open3"
require "pty"
require "rbconfig"
require "shellwords"
require "timeout"
require "tmpdir"

REFERENCE_REV = "051440ed6ae26d6cd99a33200e387f9df2462a31"
PROFILE_VARIABLES = { kward_shell: "KWSH_KWARD_CMD", plain_kwsh: "KWSH_PLAIN_CMD" }.freeze

commands = if PROFILE_VARIABLES.values.any? { |name| ENV.key?(name) }
  PROFILE_VARIABLES.transform_values do |name|
    command = Shellwords.split(ENV.fetch(name, ""))
    abort "Set both KWSH_KWARD_CMD and KWSH_PLAIN_CMD to nonempty commands" if command.empty?

    command[0] = File.expand_path(command[0]) if command[0].include?("/")
    command
  end
else
  reference = File.expand_path(ENV.fetch("KWSH_REFERENCE") { File.expand_path("../../kward", __dir__) })
  revision, error, status = Open3.capture3("git", "-C", reference, "rev-parse", "HEAD")
  abort "Cannot locate Kward reference at #{reference}: #{error}" unless status.success?
  abort "Expected Kward #{REFERENCE_REV}, got #{revision.strip} at #{reference}" unless revision.strip == REFERENCE_REV

  changes, error, status = Open3.capture3("git", "-C", reference, "status", "--porcelain", "--untracked-files=no")
  abort "Cannot check Kward reference at #{reference}: #{error}" unless status.success?
  abort "Kward reference has local changes to tracked files at #{reference}" unless changes.empty?

  {
    kward_shell: [RbConfig.ruby, File.join(reference, "exe/kward"), "shell"],
    plain_kwsh: [RbConfig.ruby, File.join(reference, "exe/kwsh")]
  }
end

class ShellParityTest < Minitest::Test
  TIMEOUT = 5
  DEFAULT_PROMPT = "parity> "
  RC_PROMPT = "parity-rc> "

  def setup
    @scratch = Dir.mktmpdir("kwsh-parity-")
    @home = File.join(@scratch, "home")
    @work = File.join(@scratch, "work")
    FileUtils.mkdir_p([@home, @work])
    File.write(File.join(@home, ".kwshrc"), "PS1='#{RC_PROMPT}'\nexport PARITY_RC=loaded\n")
    @env = {
      "HOME" => @home, "PATH" => ENV.fetch("PATH", "/usr/bin:/bin"), "LC_ALL" => "C",
      "TERM" => "xterm", "PS1" => DEFAULT_PROMPT, "HISTFILE" => ""
    }
    @spawn_options = { chdir: @work, unsetenv_others: true }
  end

  def teardown
    FileUtils.remove_entry(@scratch)
  end

  PROFILE_VARIABLES.each_key do |profile|
    define_method("test_#{profile}_command") do
      script = <<~'SH'
        printf '<%s>\n' "$0" "$@"
        printf 'diagnostic\n' >&2
        printf '%s\n' "${PARITY_RC-unset}" > artifact
        exit 17
      SH
      output, error, status = capture(*COMMANDS.fetch(profile), "-c", script, "named", "a b", "--")
      assert_equal 17, status.exitstatus, error
      assert_equal "<named>\n<a b>\n<-->\n", output
      assert_equal "diagnostic\n", error
      assert_equal "unset\n", File.binread(File.join(@work, "artifact"))
    end

    define_method("test_#{profile}_redirected_input") do
      input = <<~'SH'
        read value
        from-stdin
        printf '<%s>\n' "$value"
      SH
      output, error, status = capture(*COMMANDS.fetch(profile), input: input)
      assert status.success?, error
      assert_equal "<from-stdin>\n", output
      assert_empty error
    end

    define_method("test_#{profile}_terminal") do
      prompt = profile == :kward_shell ? RC_PROMPT : DEFAULT_PROMPT
      executable, *args = COMMANDS.fetch(profile)
      PTY.spawn(@env, [executable, executable], *args, **@spawn_options) do |reader, writer, pid|
        reader.winsize = [24, 80]
        read_until(reader, prompt)
        writer.write("printf '%s-%s\\n' parity output\n")
        read_until(reader, "\r\nparity-output\r\n")
        read_until(reader, prompt)
        writer.write("exit 0\n")
        assert wait_for_exit(pid).success?
      ensure
        reader.close
        writer.close
        cleanup_child(pid)
      end
    end
  end

  private

  def capture(executable, *args, input: "")
    stdin, stdout, stderr = %w[stdin stdout stderr].map { |name| File.join(@scratch, name) }
    File.binwrite(stdin, input)
    pid = Process.spawn(@env, [executable, executable], *args, **@spawn_options, in: stdin, out: stdout, err: stderr)
    status = wait_for_exit(pid)
    [File.binread(stdout), File.binread(stderr), status]
  ensure
    cleanup_child(pid) if pid
  end

  def read_until(reader, expected)
    tail = +"".b
    Timeout.timeout(TIMEOUT) do
      until tail.end_with?(expected)
        tail << reader.readpartial(1)
        tail = tail.byteslice(-1024, 1024) if tail.bytesize > 1024
      end
    end
  rescue Timeout::Error, EOFError, Errno::EIO
    flunk "terminal did not produce #{expected.inspect}; last output: #{tail.inspect}"
  end

  def wait_for_exit(pid)
    Timeout.timeout(TIMEOUT) { Process.waitpid2(pid).last }
  rescue Timeout::Error
    flunk "shell did not exit within #{TIMEOUT} seconds"
  end

  def cleanup_child(pid)
    return if Process.waitpid(pid, Process::WNOHANG)

    begin
      Process.kill("KILL", pid)
    rescue Errno::ESRCH
      # The child exited between the poll and signal.
    end
    Process.waitpid(pid)
  rescue Errno::ECHILD
    # Already reaped by wait_for_exit.
  end
end

ShellParityTest::COMMANDS = commands.freeze
