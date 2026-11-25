require "io/console"
require "minitest/autorun"
require "json"
require "pty"
require "rbconfig"
require "shellwords"
require "timeout"
require "tmpdir"

class TerminalSpikeTest < Minitest::Test
  SPIKE = File.expand_path(ENV.fetch("KWSH_SPIKE", File.expand_path("../target/debug/examples/terminal_spike", __dir__)))
  KWSH = File.expand_path("../target/debug/kwsh", __dir__)
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
    with_terminal(KWSH, env: { "PROMPT" => "rust> ", "TERM" => "xterm" }, prompt: "rust> ") do |reader, writer, pid|
      writer.write("exit 17\x03")
      read_until(reader, "^C\r\n")
      read_until(reader, "rust> ")
      writer.write("printf '%s-%s\\n' still here\n")
      read_until(reader, "\r\nstill-here\r\n")
      read_until(reader, "rust> ")
      exit_spike(reader, writer, pid)
    end
  end

  def test_foreground_signals_and_resume
    probe = "#{Shellwords.escape(SPIKE)} --foreground-probe\n"
    prompt = "jobs> "
    env = { "PS1" => prompt, "PROMPT" => nil, "TERM" => "xterm" }
    with_terminal(KWSH, env: env, prompt: prompt) do |reader, writer, pid|
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
      read_until(reader, "fg\e[?2004l\r\n")
      writer.write("after resume\n")
      read_until(reader, "foreground read: after resume\r\n")
      read_until(reader, prompt)
      exit_spike(reader, writer, pid)
    end
  end

  def test_pipeline_interrupts_and_resumes_as_one_foreground_job
    probe = "#{Shellwords.escape(SPIKE)} --foreground-probe"
    prompt = "pipeline> "
    with_terminal(KWSH, env: { "PROMPT" => prompt, "TERM" => "xterm" }, prompt: prompt) do |reader, writer, pid|
      writer.write("#{probe} | /bin/cat\n")
      read_until(reader, "foreground ready\r\n")
      writer.write("\x03")
      read_until(reader, prompt)

      writer.write("#{probe} | /bin/cat\n")
      read_until(reader, "foreground ready\r\n")
      writer.write("\x1a")
      read_until(reader, "Stopped\r\n")
      read_until(reader, prompt)
      writer.write("fg\n")
      read_until(reader, "fg\e[?2004l\r\n")
      writer.write("pipeline input\n")
      read_until(reader, "foreground read: pipeline input\r\n")
      read_until(reader, prompt)
      exit_spike(reader, writer, pid)
    end
  end

  def test_stopped_job_can_run_in_background_and_return_to_foreground
    prompt = "job> "
    with_terminal(KWSH, env: { "PROMPT" => prompt, "TERM" => "xterm" }, prompt: prompt) do |reader, writer, pid|
      writer.write("/bin/sleep 30\n")
      read_until(reader, "/bin/sleep 30\e[?2004l\r\n")
      # The foreground command must own the terminal before it receives Ctrl+Z.
      sleep 0.1
      writer.write("\x1a")
      read_until(reader, "Stopped\r\n")
      read_until(reader, prompt)
      writer.write("jobs\n")
      read_until(reader, /\[1\] Stopped (\d+)\r\n\z/)
      read_until(reader, prompt)
      writer.write("bg %1\n")
      read_until(reader, prompt)
      writer.write("jobs\n")
      read_until(reader, /\[1\] Running \d+\r\n\z/)
      read_until(reader, prompt)
      writer.write("fg %1\n")
      read_until(reader, "fg %1\e[?2004l\r\n")
      sleep 0.1
      writer.write("\x03")
      read_until(reader, prompt)
      writer.write("printf '%s\\n' \"$?\"\n")
      read_until(reader, "\r\n130\r\n")
      read_until(reader, prompt)
      exit_spike(reader, writer, pid)
    end
  end

  def test_startup_and_prompt
    env = { "ENV" => '$HOME/startup file', "PS1" => '$MARK! !!> ', "PROMPT" => nil, "TERM" => "xterm" }
    with_terminal(KWSH, env: env, prompt: "~ [0] %> ", setup: ->(home) {
      Dir.mkdir(File.join(home, ".kward"))
      File.write(File.join(home, ".kward/kwshrc"), "RC=primary\n")
      File.write(File.join(home, ".kwshrc"), "RC=\"$RC:personal\"\nENV=ignored\nPROMPT='%~ [%?] %%> '\n")
      File.write(File.join(home, "startup file"), "printf 'startup:%s\\n' \"$RC\"\nMARK=env\n")
      File.write(File.join(home, "ignored"), "exit 42\n")
    }, start: false) do |reader, writer, pid|
      assert_includes read_until(reader, "~ [0] %> "), "startup:primary:personal\r\n"
      writer.write("cd moved; /usr/bin/false\n")
      read_until(reader, "~/moved [1] %> ")
      writer.write("unset PROMPT\n")
      read_until(reader, "env3 !> ")
      exit_spike(reader, writer, pid)
    end
  end

  def test_private_commands_are_not_persisted_or_recalled
    env = { "PROMPT" => "hist> ", "HISTFILE" => "history.jsonl", "TERM" => "xterm" }
    with_terminal(KWSH, env: env, prompt: "hist> ") do |reader, writer, pid, home|
      writer.write("printf 'public-output\\n'\n")
      read_until(reader, "public-output\r\n")
      read_until(reader, "hist> ")
      writer.write("history pause\n")
      read_until(reader, "[private] hist> ")
      writer.write("printf 'private-output\\n'\n")
      read_until(reader, "private-output\r\n")
      read_until(reader, "[private] hist> ")
      writer.write("history resume\n")
      read_until(reader, "hist> ")
      writer.write("\e[A\n")
      read_until(reader, "public-output\r\n")
      read_until(reader, "hist> ")
      exit_spike(reader, writer, pid)
      path = File.join(home, "history.jsonl")
      assert_equal 0o600, File.stat(path).mode & 0o777
      assert_equal ["printf 'public-output\\n'", "printf 'public-output\\n'", "exit"],
        File.readlines(path).map { |line| JSON.parse(line).last }
    end
  end

  def test_bracketed_paste_is_an_editable_draft_until_enter
    with_terminal(KWSH, env: { "TERM" => "xterm", "PROMPT" => "paste> " }, prompt: "paste> ") do |reader, writer, pid, home|
      writer.write("\e[200~printf first > one\n" + "printf bad > two\e[201~")
      read_until(reader, "printf bad > two")
      refute_path_exists File.join(home, "one")
      refute_path_exists File.join(home, "two")
      writer.write("\e[D" * 6 + "\x7f" * 3 + "good\n")
      read_until(reader, "\e[?2004hpaste> ")
      assert_equal "first", File.read(File.join(home, "one"))
      assert_equal "good", File.read(File.join(home, "two"))
      writer.write("\e[200~printf unsafe > three\n\x00\e[201~")
      read_until(reader, "\a")
      refute_path_exists File.join(home, "three")
      exit_spike(reader, writer, pid)
    end
  end

  def test_tab_completion_and_word_deletion
    env = { "PROMPT" => "edit> ", "TERM" => "xterm" }
    with_terminal(KWSH, env: env, prompt: "edit> ", setup: ->(home) {
      File.write(File.join(home, "file with space"), "completed-content\n")
    }) do |reader, writer, pid|
      writer.write("/bin/cat wrong\x17fi\t\n")
      read_until(reader, "completed-content\r\n")
      read_until(reader, "edit> ")
      exit_spike(reader, writer, pid)
    end
  end

  def test_assistant_changes_live_directory_but_does_not_execute_draft
    env = { "PROMPT" => "%1~> ", "TERM" => "xterm" }
    with_terminal(KWSH, env: env, prompt: "~> ", setup: ->(home) {
      provider = File.join(home, "provider")
      File.write(provider, <<~'SCRIPT')
        #!/bin/sh
        read -r request
        case "$request" in
          *private-request*) touch private-request-sent ;;
          *waiting*) echo "$$" > waiting.pid; exec /bin/sleep 60 ;;
          *) printf '%s\n' '{"directory":"moved","draft":"printf draft > marker"}' ;;
        esac
      SCRIPT
      File.chmod(0o700, provider)
      File.write(File.join(home, ".kwshrc"), 'KWSH_ASSISTANT_EXEC="$HOME/provider"' + "\n")
    }) do |reader, writer, pid, home|
      writer.write("history pause\n")
      read_until(reader, "[private] ~> ")
      writer.write("? private-request\n")
      read_until(reader, "Shell agent unavailable while history is paused.")
      read_until(reader, "[private] ~> ")
      refute File.exist?(File.join(home, "private-request-sent"))
      writer.write("history resume\n")
      read_until(reader, "~> ")

      writer.write("? waiting\n")
      provider_pid = Timeout.timeout(TIMEOUT) do
        path = File.join(home, "waiting.pid")
        loop do
          value = File.read(path).strip if File.exist?(path)
          break Integer(value) if value && !value.empty?
          sleep 0.01
        end
      end
      writer.write("\x03")
      read_until(reader, "turn cancelled\r\n")
      read_until(reader, "~> ")
      assert_raises(Errno::ESRCH) { Process.kill(0, provider_pid) }

      writer.write("? make a draft\n")
      read_until(reader, "moved> printf draft > marker")
      marker = File.join(home, "moved", "marker")
      refute File.exist?(marker)
      writer.write("-edited\n")
      read_until(reader, "moved> ")
      assert_equal "draft", File.read(marker + "-edited")
      refute File.exist?(marker)
      exit_spike(reader, writer, pid)
    end
  end

  def test_assistant_receives_last_command_and_live_cwd_but_not_private_context
    with_context_provider do |reader, writer, pid, home, prompt|
      writer.write("cd moved\n")
      read_until(reader, prompt)
      command = %q(/bin/sh -c 'echo visible; echo fixture-failure >&2; echo fd-nine >&9; exit 23')
      writer.write("#{command}\n")
      read_until(reader, "\r\nvisible\r\nfixture-failure\r\n")
      read_until(reader, prompt)

      request = assistant_request(reader, writer, home, prompt)
      assert_equal "inspect context", request.fetch("instruction")
      assert_equal({
        "cwd" => File.realpath(File.join(home, "moved")),
        "last_command" => command,
        "last_output" => "visible\nfixture-failure\n",
        "exit_status" => 23
      }, request.fetch("context"))
      assert_equal "fd-nine\n", File.read(File.join(home, "fd9"))

      writer.write("history pause\n")
      read_until(reader, "[private] #{prompt}")
      writer.write("printf 'private-data\\n'\n")
      read_until(reader, "\r\nprivate-data\r\n")
      read_until(reader, "[private] #{prompt}")
      writer.write("history resume\n")
      read_until(reader, prompt)
      context = assistant_request(reader, writer, home, prompt).fetch("context")
      assert_nil context.fetch("last_command")
      assert_nil context.fetch("last_output")
      assert_nil context.fetch("exit_status")
      exit_spike(reader, writer, pid)
    end
  end

  def test_assistant_omits_output_from_a_job_that_reads_terminal_input
    with_context_provider do |reader, writer, pid, home, prompt|
      command = %q(/bin/sh -c 'echo waiting-for-input; read text; echo "$text"')
      writer.write("#{command}\n")
      read_until(reader, "\r\nwaiting-for-input\r\n")
      writer.write("typed-data\n")
      read_until(reader, "typed-data\r\ntyped-data\r\n")
      read_until(reader, prompt)
      context = assistant_request(reader, writer, home, prompt).fetch("context")
      assert_nil context.fetch("last_output")
      assert_equal command, context.fetch("last_command")
      assert_equal 0, context.fetch("exit_status")
      exit_spike(reader, writer, pid)
    end
  end

  private

  def with_context_provider
    prompt = "context> "
    with_terminal(KWSH, env: { "PROMPT" => prompt, "TERM" => "xterm" }, prompt: prompt, setup: ->(home) {
      provider = File.join(home, "provider")
      File.write(provider, <<~'SCRIPT')
        #!/bin/sh
        /bin/cat > "$HOME/request.json"
        printf '%s\n' '{"answer":"request captured"}'
      SCRIPT
      File.chmod(0o700, provider)
      File.write(File.join(home, ".kwshrc"), 'KWSH_ASSISTANT_EXEC="$HOME/provider"' + "\n")
    }) do |reader, writer, pid, home|
      yield reader, writer, pid, home, prompt
    end
  end

  def assistant_request(reader, writer, home, prompt)
    writer.write("? inspect context\n")
    read_until(reader, "\r\nrequest captured\r\n")
    read_until(reader, prompt)
    JSON.parse(File.read(File.join(home, "request.json")))
  end

  def with_spike(&block)
    with_terminal(SPIKE, env: {}, prompt: PROMPT, &block)
  end

  def with_terminal(executable, *args, env:, prompt:, setup: nil, start: true)
    Dir.mktmpdir("kwsh-terminal-") do |work|
      Dir.mkdir(File.join(work, "moved"))
      setup&.call(work)
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
          read_until(reader, prompt) if start
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
