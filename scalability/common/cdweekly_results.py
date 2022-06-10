#!/bin/python
import glob
import json
import os
import subprocess
import sys
import time
import traceback
from datetime import datetime

import gflags
import pybars

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from common import misc  # noqa

FLAGS = gflags.FLAGS
gflags.DEFINE_string("experiment_data", ".", "Path to experiment data")
gflags.DEFINE_boolean("regenerate", False, "Regenerate all reports")

TEMPLATE_PATH = "templates/cd-overview.html.hb"


def convert_date(ts: int):
    # Also works in plotly: https://plotly.com/javascript/time-series/
    return datetime.utcfromtimestamp(ts).strftime("%Y-%m-%d %H:%M:%S")


WATTS_PER_NODE = 700
MAINNET_CURR_TRANSACTION_RATE = 3300  # per second
# Total facility enery (incl compute + cooling) / IT equipment energy (only compute)
# https://en.wikipedia.org/wiki/Power_usage_effectiveness
PUE = 2.33


def parse_rps_experiment(data, githash, timestamp):
    xvalue = data["t_experiment_start"]
    yvalue = data["experiment_details"]["rps_max"]
    meta_data = {
        "timestamp": timestamp,
        "date": convert_date(int(timestamp)),
        "githash": githash,
        "yvalue": yvalue,
        "xvalue": xvalue,
    }

    print(
        "  {:40} {:30} {:10.3f}".format(
            data["experiment_name"],
            convert_date(data["t_experiment_start"]),
            float(data["experiment_details"]["rps_max"]),
        )
    )
    raw_data = (xvalue, yvalue)
    return (meta_data, raw_data)


def parse_xnet_experiment(data, githash, timestamp):
    xvalue = data["t_experiment_start"]

    if "max_capacity" in data["experiment_details"]:
        yvalue = data["experiment_details"]["max_capacity"]
        meta_data = {
            "timestamp": timestamp,
            "date": convert_date(int(timestamp)),
            "githash": githash,
            "yvalue": yvalue,
            "xvalue": xvalue,
        }

        print(
            "  {:40} {:30} {:10.3f}".format(data["experiment_name"], convert_date(data["t_experiment_start"]), yvalue)
        )
        raw_data = (xvalue, yvalue)
        return (meta_data, raw_data)
    else:
        return (None, None)


def parse_statesync_experiment(data, githash, timestamp):
    xvalue = data["t_experiment_start"]
    yvalue = data["experiment_details"]["state_sync_duration"]
    meta_data = {
        "timestamp": timestamp,
        "date": convert_date(int(timestamp)),
        "githash": githash,
        "yvalue": yvalue,
        "xvalue": xvalue,
    }

    # Some older versions of the experiment data have Prometheus metrics as values,
    # instead of the extracted float value.
    if type(yvalue) is dict:
        yvalue = yvalue["result"][0]["value"][1]
    yvalue = float(yvalue)

    print("  {:40} {:30} {:10.3f}".format(data["experiment_name"], convert_date(data["t_experiment_start"]), yvalue))
    raw_data = (xvalue, yvalue)
    return (meta_data, raw_data)


def find_results(experiment_names, experiment_type, parser, threshold, testnet="cdslo", time_start=None):
    """Find and collect data from all experiments for the given testnet and experiment type."""
    meta_data = []
    raw_data = []
    # Find all experiments
    for result in glob.glob(f"{FLAGS.experiment_data}/*/*/experiment.json"):
        with open(result) as resultfile:
            try:
                data = json.loads(resultfile.read())
                # Match testnet name, experiment name and experiment type in order to decide whether to include results
                if data["testnet"] == testnet and data["type"] in experiment_type:

                    include = False
                    for experiment in experiment_names:
                        include = include or data["experiment_name"] == experiment
                    if not include:
                        continue

                    if time_start is None or int(data["t_experiment_start"]) > time_start:

                        githash, timestamp, _ = tuple(result.split("/")[-3:])
                        new_meta_data, new_raw_data = parser(data, githash, timestamp)
                        if new_meta_data is not None and new_raw_data is not None:
                            meta_data.append(new_meta_data)
                            raw_data.append(new_raw_data)

                            try:
                                if (
                                    os.path.isfile(f"{FLAGS.experiment_data}/{githash}/{timestamp}/report.html")
                                    and not FLAGS.regenerate
                                ):
                                    print("✅ Report exists")
                                else:
                                    print("⚠️  Report does not exists yet, generating")
                                    subprocess.check_output(
                                        [
                                            "python3",
                                            "common/generate_report.py",
                                            "--git_revision",
                                            githash,
                                            "--timestamp",
                                            timestamp,
                                            "--base_dir",
                                            FLAGS.experiment_data,
                                        ]
                                    )
                            except Exception as e:
                                print(f"❌ Failed to generate report: {e}")

            except Exception as e:
                print(traceback.format_exc())
                print(f"Failed to check ${result} - error: {e}")

    if len(raw_data) < 1:
        raise Exception(f"Could not find any data for: {testnet} {experiment_names} {experiment_type}")

    raw_data = sorted(raw_data)
    meta_data = sorted(meta_data, key=lambda x: x["timestamp"])

    xdata = [e[0] for e in raw_data]
    ydata = [e[1] for e in raw_data]

    plots = [
        {
            "x": [convert_date(e) for e in xdata],
            "y": ydata,
        }
    ]

    layout = {
        "yaxis": {"title": "maximum rate [requests / s]", "range": [0, 1.2 * max(ydata)]},
        "xaxis": {"title": "benchmark execution date [s]"},
        "shapes": [
            {
                "type": "line",
                "x0": convert_date(min(xdata)),
                "y0": threshold,
                "x1": convert_date(max(xdata)),
                "y1": threshold,
                "line": {
                    "color": "red",
                },
            }
        ],
    }

    return {"plot": plots, "layout": layout, "data": meta_data}


def get_num_nodes_mainnet():

    sys.path.insert(1, ".")
    from common.base_experiment import BaseExperiment

    nns_url = BaseExperiment.get_mainnet_nns_url()

    # Maybe deduplicate with experiment.py
    res = subprocess.check_output(
        ["../artifacts/release/ic-admin", "--nns-url", f"http://[{nns_url}]:8080", "get-topology"],
        encoding="utf-8",
    )

    return parse_topo(res)


def parse_topo(data):

    data = json.loads(data)

    subnets = data["topology"]["subnets"]
    num_subnets = 0
    num_nodes = 0

    num_app_subnets = 0
    num_app_nodes = 0

    for (_, v) in subnets.items():
        subnet_type = v["records"][0]["value"]["subnet_type"]
        print("Subnet type:", subnet_type)
        num_subnets += 1
        num_nodes += len(v["records"][0]["value"]["membership"])
        if subnet_type != "system":
            num_app_subnets += 1
            num_app_nodes += len(v["records"][0]["value"]["membership"])

    return (num_nodes, num_subnets, num_app_nodes, num_app_subnets)


def get_num_boundary_nodes():
    sys.path.insert(1, ".")
    from common import prometheus

    r = prometheus.get_prometheus({"query": 'count(nginx_up{ic="mercury"})'})
    return tuple(prometheus.extract_value(json.loads(r.text))[0])


if __name__ == "__main__":

    misc.parse_command_line_args()
    num_nodes, num_subnets, num_app_nodes, num_app_subnets = get_num_nodes_mainnet()
    timestamp, num_boundary_nodes = get_num_boundary_nodes()
    num_boundary_nodes = int(num_boundary_nodes)
    print("Boundary nodes at ", datetime.fromtimestamp(timestamp), num_boundary_nodes)

    with open(TEMPLATE_PATH, mode="r") as f:
        compiler = pybars.Compiler()
        source = f.read()
        template = compiler.compile(source)

        data = {
            "num_subnets": num_subnets,
            "num_nodes": num_nodes,
            "num_app_subnets": num_app_subnets,
            "num_app_nodes": num_app_nodes,
            "num_boundary_nodes": num_boundary_nodes,
            "last_generated": int(time.time()),
        }

        print("Experiment 1")
        data["plot_exp1_query"] = find_results(
            ["experiment_1", "run_system_baseline_experiment", "system-baseline-experiment"],
            ["query"],
            parse_rps_experiment,
            2800,
        )
        data["plot_exp1_query"]["layout"].update(
            {
                "annotations": [
                    {
                        "x": convert_date(1639340737),
                        "y": "1750.0",
                        "xref": "x",
                        "yref": "y",
                        "text": "workload generator http1 only",
                        "textangle": "-60",
                        "showarrow": "true",
                        "arrowhead": 7,
                        "ax": 0,
                        "ay": -40,
                    },
                    {
                        "x": convert_date(1642271520),
                        "y": "4000.0",
                        "xref": "x",
                        "yref": "y",
                        "text": "likely EXC-832",
                        "textangle": "-60",
                        "showarrow": "true",
                        "arrowhead": 7,
                        "ax": 0,
                        "ay": -40,
                    },
                ]
            }
        )
        data["plot_exp1_update"] = find_results(
            ["experiment_1", "run_system_baseline_experiment", "system-baseline-experiment"],
            ["update"],
            parse_rps_experiment,
            500,
        )

        # Calculate theoretical stats from latest system overhead experiments
        latest_query_performance = data["plot_exp1_query"]["plot"][0]["y"][-1]
        latest_update_performance = data["plot_exp1_update"]["plot"][0]["y"][-1]
        print("query", data["plot_exp1_query"]["plot"][0]["y"], latest_query_performance)
        print("update", latest_update_performance)

        latest_approx_mainnet_update_performance = num_app_subnets * latest_update_performance
        latest_approx_mainnet_query_performance = num_app_nodes * latest_query_performance

        data["latest_approx_mainnet_subnet_update_performance"] = "{:.0f}".format(latest_update_performance)
        data["latest_approx_mainnet_node_query_performance"] = "{:.0f}".format(latest_query_performance)

        data["latest_approx_mainnet_update_performance"] = "{:.0f}".format(latest_approx_mainnet_update_performance)
        data["latest_approx_mainnet_query_performance"] = "{:.0f}".format(latest_approx_mainnet_query_performance)

        data["watts_per_node"] = WATTS_PER_NODE
        watts_per_node_total = WATTS_PER_NODE * PUE
        data["watts_per_node_total"] = watts_per_node_total
        watts_ic = watts_per_node_total * (num_nodes + num_boundary_nodes)
        data["watts_ic"] = "{:.0f}".format(watts_ic)

        data["pue"] = PUE

        joules_per_update_at_capacity = watts_ic / (latest_approx_mainnet_update_performance)
        joules_per_query_at_capacity = watts_ic / (latest_approx_mainnet_query_performance)
        data["joules_per_update_at_capacity"] = "{:.2f}".format(joules_per_update_at_capacity)
        data["joules_per_query_at_capacity"] = "{:.2f}".format(joules_per_query_at_capacity)

        data["transaction_current"] = MAINNET_CURR_TRANSACTION_RATE
        joules_per_transaction_current = watts_ic / MAINNET_CURR_TRANSACTION_RATE
        data["joules_per_transaction_current"] = "{:.2f}".format(joules_per_transaction_current)

        print("Experiment 2")
        data["plot_exp2_update"] = find_results(
            ["experiment_2", "run_large_memory_experiment"],
            ["update", "update_copy"],
            parse_rps_experiment,
            20,
            time_start=1639939557,
        )
        data["plot_exp2_query"] = find_results(
            ["experiment_2", "run_large_memory_experiment"], ["query", "query_copy"], parse_rps_experiment, 150
        )
        data["plot_statesync"] = find_results(["run_statesync_experiment"], ["query"], parse_statesync_experiment, 2.2)
        data["plot_xnet"] = find_results(["run_xnet_experiment"], ["query"], parse_xnet_experiment, 5500)
        print(data)

        with open(f"{FLAGS.experiment_data}/cd-overview.html", "w") as outfile:
            outfile.write(template(data))
            print("🎉 Report written")
