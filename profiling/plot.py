#!/usr/bin/env python3
import pandas as pd
import matplotlib
import matplotlib.pyplot as plt
import argparse

parser = argparse.ArgumentParser(description="Plot two CSV results for comparison.")
parser.add_argument("input1", type=str, help="First CSV result file")
parser.add_argument("input2", type=str, help="Second CSV result file")
parser.add_argument("outputprefix", type=str, help="Prefix to use for the PNG graph files")
parser.add_argument("--logy", dest="logy", action="store_true", help="Use logarithmic scale for Y axis")
parser.set_defaults(logy=False)
args = parser.parse_args()

indices = ["Test", " target req/s", " req len"]
g = pd.concat([pd.read_csv(args.input1, index_col=indices), pd.read_csv(args.input2, index_col=indices)])
g.groupby(level=indices)

try:
  only_gbits = g[[" branch", " GBit/s"]][ g[" GBit/s"] > 0 ]
  only_gbits[[" branch"]] = only_gbits[[" branch"]].applymap(lambda x: ' '.join(x.split(' ')[:3]) ) # strip HTTP/gRPC info
  rearrange_gbits = only_gbits.pivot_table(index = ["Test"], columns = " branch", values = " GBit/s")
  rearrange_gbits.plot(kind="bar", title="Throughput (GBit/s)", figsize=(10, 8))
  plt.xticks(rotation = 0)
  outfile_gbits = args.outputprefix + "gbits.png"
  print("Save graph to", outfile_gbits)
  plt.savefig(outfile_gbits, bbox_inches="tight")
  print("Plotted TCP graph sucessfully")
except Exception as e:
  print("Error:", e)

try:
  only_latency = g[[" branch", " p999 latency (ms)"]][ g[" p999 latency (ms)"] > 0 ]
  rearrange_latency = only_latency.pivot_table(index=indices, columns=" branch", values=" p999 latency (ms)")
  rearrange_latency.plot(kind="bar", logy=args.logy, title="p999 Latency (ms)", figsize=(28, 3), fontsize=7) # increase figsize x value if labels overlap
  plt.xticks(rotation = 0)
  outfile_latency = args.outputprefix + "latency.png"
  print("Save graph to", outfile_latency)
  plt.savefig(outfile_latency, bbox_inches="tight")
  print("Plotted HTTP/gRPC graph sucessfully")
except Exception as e:
  print("Error:", e)
