import os

import gflags
import metrics
import ssh

FLAGS = gflags.FLAGS
gflags.DEFINE_boolean("no_flamegraphs", False, "Set true to disable generating flamegraphs.")


class Flamegraph(metrics.Metric):
    """Flamegraph abstraction. Can be started and stopped."""

    def init(self):
        """
        Init the metrics.

        Called once at the beginning of the benchmark.
        """
        if FLAGS.no_flamegraphs:
            self.do_instrument = False
            return
        self.install_flamegraph([self.target])

    def install_flamegraph(self, machines):
        """
        Install flamegraph binaries if not yet available.

        cargo install flamegraph --git https://github.com/flamegraph-rs/flamegraph --branch main

        This will only work if the machine you install on has IPv4 support (for github), which is not
        true for the IC OS.
        If you build on a non-IC OS machine, be sure to have compatible libc etc.
        """
        if not self.do_instrument:
            return
        r = ssh.run_ssh_in_parallel(machines, "stat flamegraph")
        if r != [0 for _ in machines]:

            # Flamegraph binary not installed: installing and setting up OS.

            # Could also think about doing this:
            # warning: Maximum frequency rate (750 Hz) exceeded, throttling from 997 Hz to 750 Hz.
            # The limit can be raised via /proc/sys/kernel/perf_event_max_sample_rate.
            # The kernel will lower it when perf's interrupts take too long.

            ssh.run_ssh_in_parallel(machines, "echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid")
            ssh.run_ssh_in_parallel(
                machines, "sudo apt update; sudo apt install -y linux-tools-common linux-tools-$(uname -r)"
            )

            destinations = ["admin@[{}]:".format(m) for m in machines]
            sources = ["flamegraph" for _ in machines]
            return ssh.scp_in_parallel(sources, destinations)

        else:
            return r

    def start_iteration(self, outdir):
        """Benchmark iteration is started."""
        if not self.do_instrument:
            return
        self.flamegraph_pid = ssh.run_ssh_with_t(
            self.target,
            (
                "sudo rm -f /tmp/flamegraph.svg; "
                "rm -f perf.data perf.data.old; "
                "cp -f flamegraph /tmp; cd /tmp; "
                "sudo ./flamegraph -p $(pidof replica) --root --no-inline -o /tmp/flamegraph.svg"
            ),
            os.path.join(outdir, "flamegraph-{}.stdout.log".format(self.target)),
            os.path.join(outdir, "flamegraph-{}.stderr.log".format(self.target)),
        )

    def end_iteration(self, exp):
        """Benchmark iteration is started."""
        if not self.do_instrument:
            return
        print("Terminating flamegraph generation, waiting to finish and fetching svg.")
        # It's insufficient to terminate() flamegraph itself.
        # We need to either send SIGINT to the entire process group, or simply terminate perf itself.
        # That will trigger flamegraph to start generating the flamegraph binary, which we then have
        # to wait for.
        ssh.run_ssh(self.target, "sudo kill $(pidof perf)")
        r = self.flamegraph_pid.wait()
        r = ssh.scp_file(
            f"admin@[{self.target}]:/tmp/flamegraph.svg", f"{exp.iter_outdir}/flamegraph_{self.target}.svg"
        ).wait()
        if r != 0:
            print("❌ Failed to fetch flamegraph, continuing")
        else:
            print("Waiting for flamegraph done .. success")


if __name__ == "__main__":

    # Useful for more lightweight testing and development.
    # Should normally not be ran directly.

    import subprocess
    import time
    import threading

    import experiment
    import gflags
    import sys

    gflags.FLAGS(sys.argv)

    exp = experiment.Experiment()
    exp.start_iteration()

    def thread():
        for i in range(50):
            subprocess.run(["echo", "hello", i])
            time.sleep(10)

    for i in range(10):

        th = threading.Thread(target=thread)
        th.start()

        m = Flamegraph("flamegraph", exp.target)
        m.init()
        m.start_iteration("/tmp")
        time.sleep(500)
        m.end_iteration(exp)

        th.join()
