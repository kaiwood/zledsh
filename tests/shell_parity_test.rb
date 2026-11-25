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
command = if ENV.key?("KWSH_CMD")
  argv = Shellwords.split(ENV.fetch("KWSH_CMD"))
  abort "Set KWSH_CMD to a nonempty command" if argv.empty?

  argv[0] = File.expand_path(argv[0]) if argv[0].include?("/")
  argv
else
  reference = File.expand_path(ENV.fetch("KWSH_REFERENCE") { File.expand_path("../../kward", __dir__) })
  revision, error, status = Open3.capture3("git", "-C", reference, "rev-parse", "HEAD")
  abort "Cannot locate Kward reference at #{reference}: #{error}" unless status.success?
  abort "Expected Kward #{REFERENCE_REV}, got #{revision.strip} at #{reference}" unless revision.strip == REFERENCE_REV

  changes, error, status = Open3.capture3("git", "-C", reference, "status", "--porcelain", "--untracked-files=no")
  abort "Cannot check Kward reference at #{reference}: #{error}" unless status.success?
  abort "Kward reference has local changes to tracked files at #{reference}" unless changes.empty?

  # Only the legacy Ruby reference uses this launcher.
  [RbConfig.ruby, File.join(reference, "exe/kward"), "shell"]
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

  def test_command
    script = <<~'SH'
      printf '<%s>\n' "$0" "$@"
      printf 'diagnostic\n' >&2
      printf '%s\n' "${PARITY_RC-unset}" > artifact
      exit 17
    SH
    output, error, status = capture(*COMMAND, "-c", script, "named", "a b", "--")
    assert_equal 17, status.exitstatus, error
    assert_equal "<named>\n<a b>\n<-->\n", output
    assert_equal "diagnostic\n", error
    assert_equal "unset\n", File.binread(File.join(@work, "artifact"))
  end

  def test_redirected_input
    input = <<~'SH'
      read value
      from-stdin
      printf '<%s>\n' "$value"
    SH
    output, error, status = capture(*COMMAND, input: input)
    assert status.success?, error
    assert_equal "<from-stdin>\n", output
    assert_empty error
  end

  def test_startup_style_nested_parameter_defaults
    script = <<~'SH'
      unset RBENV_ROOT
      printf '%s\n' "${RBENV_ROOT:-$HOME/.rbenv}/shims"
      RBENV_ROOT=
      printf '<%s> <%s>\n' "${RBENV_ROOT-fallback}" "${RBENV_ROOT:-fallback}"
    SH
    output, error, status = capture(*COMMAND, "-c", script)
    assert status.success?, error
    assert_equal "#{@home}/.rbenv/shims\n<> <fallback>\n", output
    assert_empty error
  end

  def test_simple_aliases_use_the_next_input_unit
    script = <<~'SH'
      alias say='printf old'
      alias say='printf new'; say
      say
      alias printf='false'
      "printf" ':literal'
      unalias printf
      printf ':removed'
    SH
    output, error, status = capture(*COMMAND, "-c", script)
    assert status.success?, error
    assert_equal "oldnew:literal:removed", output
    assert_empty error
  end

  def test_background_pipeline_waits_for_its_last_status
    script = <<~'SH'
      /usr/bin/true | /usr/bin/false &
      saved="$!"
      printf '%s\n' launched
      wait "$saved"
      printf '%s\n' "$?"
    SH
    output, error, status = capture(*COMMAND, "-c", script)
    assert status.success?, error
    assert_equal "launched\n1\n", output
    assert_empty error
  end

  def test_conditionals_in_command_and_redirected_input
    script = <<~'SH'
      if /usr/bin/false; then
        printf wrong > skipped
      elif /usr/bin/true; then
        if /usr/bin/true; then selected=right; else selected=wrong; fi
      fi
      printf '%s:%s\n' "$selected" "$?"
      if /usr/bin/false; then printf wrong; fi
      printf '%s\n' "$?"
    SH
    [capture(*COMMAND, "-c", script), capture(*COMMAND, input: script)].each do |output, error, status|
      assert status.success?, error
      assert_equal "right:0\n0\n", output
      assert_empty error
    end
    refute File.exist?(File.join(@work, "skipped"))
  end

  def test_startup_and_cancelled_interactive_conditionals
    File.open(File.join(@home, ".kwshrc"), "a") do |rc|
      rc.puts "if /usr/bin/true; then export PARITY_IF=loaded; fi"
    end
    executable, *args = COMMAND
    PTY.spawn(@env, [executable, executable], *args, **@spawn_options) do |reader, writer, pid|
      read_until(reader, RC_PROMPT)
      writer.write("PS2='more> '; printf '%s\\n' \"$PARITY_IF\"\n")
      read_until(reader, "\r\nloaded\r\n")
      read_until(reader, RC_PROMPT)
      writer.write("if /usr/bin/true; then printf not-yet > skipped\n")
      read_until(reader, "more> ")
      refute File.exist?(File.join(@work, "skipped"))
      writer.write("\x03")
      read_until(reader, RC_PROMPT)
      writer.write("exit 0\n")
      drain_terminal(reader)
      assert wait_for_exit(pid).success?
      refute File.exist?(File.join(@work, "skipped"))
    ensure
      reader.close
      writer.close
      cleanup_child(pid)
    end
  end

  def test_terminal
    executable, *args = COMMAND
    PTY.spawn(@env, [executable, executable], *args, **@spawn_options) do |reader, writer, pid|
      reader.winsize = [24, 80]
      read_until(reader, RC_PROMPT)
      writer.write("printf '%s-%s\\n' parity output\n")
      read_until(reader, "\r\nparity-output\r\n")
      read_until(reader, RC_PROMPT)
      writer.write("exit 0\n")
      drain_terminal(reader)
      assert wait_for_exit(pid).success?
    ensure
      reader.close
      writer.close
      cleanup_child(pid)
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

  def drain_terminal(reader)
    # The shell may drain tty output before restoring its settings on exit.
    Timeout.timeout(TIMEOUT) { loop { reader.readpartial(4096) } }
  rescue EOFError, Errno::EIO
    nil
  rescue Timeout::Error
    flunk "terminal did not close after exit"
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

ShellParityTest::COMMAND = command.freeze
