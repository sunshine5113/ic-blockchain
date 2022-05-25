#!/usr/bin/env python3
import os
import sys

import gflags

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from common import misc  # noqa
import run_system_baseline_experiment  # noqa

FLAGS = gflags.FLAGS

gflags.DEFINE_integer("initial_rps", 100, "Starting number for requests per second.")
gflags.DEFINE_integer("increment_rps", 50, "Increment of requests per second per round.")
gflags.DEFINE_integer(
    "max_rps", 40000, "Maximum requests per second to be sent. Experiment will wrap up beyond this number."
)

if __name__ == "__main__":
    misc.parse_command_line_args()

    datapoints = misc.get_datapoints(FLAGS.target_rps, FLAGS.initial_rps, FLAGS.max_rps, FLAGS.increment_rps, 1)
    exp = run_system_baseline_experiment.BaselineExperiment()
    exp.run_iterations(datapoints)
