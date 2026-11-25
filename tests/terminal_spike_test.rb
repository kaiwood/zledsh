require "io/console"
require "minitest/autorun"
require "pty"
require "rbconfig"
require "shellwords"
require "timeout"
require "tmpdir"

class TerminalSpikeTest < Minitest::Test
  SPIKE = File.expand_path(ENV.fetch("KWSH_SPIKE", File.expand_path("../target/debug/examples/terminal_spike", __dir__)))
  PROMPT = "spike> "
  TIMEOUT = 5

  def test_foreground_stop_resume_and_restore
    with_spike do |reader, writer, pid|
      writer.write("job\n")
      read_until(reader, "foreground ready\r\n")
      writer.write("\x1a") # Ctrl+Z must go to the child's foreground group.
      read_until(reader, "foreground job stopped\r\n")
      read_until(reader, PROMPT)
      writer.write("fg\n")
      read_until(reader, "fg\r\n")
      writer.write("resumed\n")
      read_until(reader, "foreground read: resumed\r\n")
      read_until(reader, PROMPT)
      exit_spike(reader, writer, pid)
    end
  end

  def test_foreground_job_inherits_descriptor_nine
    with_spike do |reader, writer, pid, work|
      writer.write("fd\n")
      read_until(reader, PROMPT)
      assert_equal "from foreground fd 9\n", File.binread(File.join(work, "fd9"))
      exit_spike(reader, writer, pid)
    end
  end

  def test_provider_changes_live_cwd_and_stages_editable_draft
    with_spike do |reader, writer, pid, work|
      writer.write("?\n")
      read_until(reader, "#{PROMPT}mark draft")
      marker = File.join(work, "moved", "marker")
      refute_path_exists marker
      writer.write("\x7f\x7f\x7f\x7f\x7fedited\n")
      read_until(reader, "marked\r\n")
      read_until(reader, PROMPT)
      assert_equal "edited", File.binread(marker)
      refute_path_exists File.join(work, "marker")
      writer.write("pwd\n")
      read_until(reader, "cwd: #{File.realpath(work)}/moved\r\n")
      read_until(reader, PROMPT)
      exit_spike(reader, writer, pid)
    end
  end

  def test_ctrl_c_reaps_provider_and_keeps_shell_usable
    with_spike do |reader, writer, pid, work|
      writer.write("? wait\n")
      message = read_until(reader, /provider waiting: (\d+)\r\n\z/)
      provider_pid = Integer(message[/provider waiting: (\d+)/, 1])
      assert_equal 1, Process.kill(0, provider_pid)
      writer.write("\x03")
      read_until(reader, "turn cancelled\r\n")
      read_until(reader, PROMPT)
      assert_raises(Errno::ESRCH) { Process.kill(0, provider_pid) }
      writer.write("pwd\n")
      read_until(reader, "cwd: #{File.realpath(work)}\r\n")
      read_until(reader, PROMPT)
      exit_spike(reader, writer, pid)
    end
  end

  def test_real_interpreter_cancels_draft_and_restores_terminal
    kward = File.expand_path("../target/debug/kward", __dir__)
    with_terminal(kward, "shell", env: { "PROMPT" => "rust> ", "TERM" => "xterm" }, prompt: "rust> ") do |reader, writer, pid|
      writer.write("exit 17\x03")
      read_until(reader, "^C\r\n")
      read_until(reader, "rust> ")
      writer.write("printf '%s-%s\\n' still here\n")
      read_until(reader, "\r\nstill-here\r\n")
      read_until(reader, "rust> ")
      exit_spike(reader, writer, pid)
    end
  end

  { kwsh: [], kward: ["shell"] }.each do |name, args|
    define_method("test_#{name}_foreground_signals_and_resume") do
      binary = File.expand_path("../target/debug/#{name}", __dir__)
      probe = "#{Shellwords.escape(SPIKE)} --foreground-probe\n"
      prompt = "jobs> "
      env = { "PS1" => prompt, "PROMPT" => nil, "TERM" => "xterm" }
      with_terminal(binary, *args, env: env, prompt: prompt) do |reader, writer, pid|
        writer.write(probe)
        read_until(reader, "foreground ready\r\n")
        writer.write("\x03")
        read_until(reader, prompt)

        writer.write(probe)
        read_until(reader, "foreground ready\r\n")
        writer.write("\x1a")
        read_until(reader, "Stopped\r\n")
        read_until(reader, prompt)
        writer.write("fg\n")
        read_until(reader, "fg\r\n")
        writer.write("after resume\n")
        read_until(reader, "foreground read: after resume\r\n")
        read_until(reader, prompt)
        exit_spike(reader, writer, pid)
      end
    end
  end

  private

  def with_spike(&block)
    with_terminal(SPIKE, env: {}, prompt: PROMPT, &block)
  end

  def with_terminal(executable, *args, env:, prompt:)
    Dir.mktmpdir("kwsh-terminal-") do |work|
      Dir.mkdir(File.join(work, "moved"))
      # Keep the session leader alive: macOS revokes the tty when it exits.
      bootstrap = <<~'RUBY'
        def tty_modes
          modes = IO.popen(["/bin/stty", "-g"], &:read).strip
          abort "cannot read tty modes" unless $?.success?
          modes
        end

        before = tty_modes
        program, *args = ARGV
        success = system([program, program], *args, 9 => 9)
        after = tty_modes
        abort "tty modes changed: #{before} -> #{after}" unless before == after
        puts "tty restored"
        exit success ? 0 : 1
      RUBY
      File.open(File.join(work, "fd9"), "w") do |descriptor|
        PTY.spawn({ "HOME" => work, "KWARD_CONFIG_PATH" => nil }.merge(env), RbConfig.ruby, "-e", bootstrap, executable, *args,
                  chdir: work, 9 => descriptor) do |reader, writer, pid|
          reader.winsize = [24, 80]
          read_until(reader, prompt)
          yield reader, writer, pid, work
        ensure
          reader.close
          writer.close
          reap(pid)
        end
      end
    end
  end

  def read_until(reader, expected)
    output = +"".b
    Timeout.timeout(TIMEOUT) do
      until expected.is_a?(Regexp) ? expected.match?(output) : output.end_with?(expected)
        output << reader.readpartial(1)
        output = output.byteslice(-2048, 2048) if output.bytesize > 2048
      end
    end
    output
  rescue Timeout::Error, EOFError, Errno::EIO
    flunk "terminal did not produce #{expected.inspect}; last output: #{output.inspect}"
  end

  def exit_spike(reader, writer, pid)
    writer.write("exit\n")
    read_until(reader, "tty restored\r\n")
    status = Timeout.timeout(TIMEOUT) { Process.waitpid2(pid).last }
    assert status.success?, status.inspect
  rescue Timeout::Error
    flunk "spike did not exit within #{TIMEOUT} seconds"
  end

  def reap(pid)
    return if Process.waitpid(pid, Process::WNOHANG)

    Process.kill("KILL", pid)
    Process.waitpid(pid)
  rescue Errno::ECHILD, Errno::ESRCH
    nil
  end
end
