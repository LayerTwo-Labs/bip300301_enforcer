#!/usr/bin/env python3

# /// script
# requires-python = ">=3.9"
# dependencies = [
#     "matplotlib>=3.5.0",
#     "pandas>=1.3.0",
# ]
# ///

"""
Simple BIP300301 Enforcer Sync Progress Analyzer

Shows a single graph of block height vs relative time from sync logs.

Usage:
    uv run analyze_sync_logs.py <log_file_path>
"""

import argparse
import re
import sys
from datetime import datetime
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd


def parse_timestamp(timestamp_str: str) -> datetime:
    """Parse ISO timestamp from log line."""
    return datetime.fromisoformat(timestamp_str.replace("Z", "+00:00"))


def extract_sync_progress(log_file: Path) -> tuple[pd.DataFrame, str, str]:
    """Extract sync progress, git hash, and build type from log file in a single pass."""
    events = []
    sync_starts = []
    git_hash = "unknown"
    build_type = "unknown"

    # Pattern to match sync progress: "updated current chain tip: N"
    progress_pattern = re.compile(
        r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z).*updated current chain tip: (\d+)"
    )

    # Pattern to match sync start: "identified X missing blocks in Y, starting sync"
    sync_start_pattern = re.compile(
        r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z).*identified (\d+) missing blocks in ([^,]+), starting sync"
    )

    # Pattern to match startup line
    startup_pattern = re.compile(r".*Starting up bip300301_enforcer.*")

    # Patterns to extract individual fields from startup line
    git_hash_pattern = re.compile(r'git_hash="([^"]+)"')
    build_type_pattern = re.compile(r'build="([^"]+)"')

    print(f"📖 Reading log file: {log_file}")

    with open(log_file, "r", encoding="utf-8") as f:
        for line_num, line in enumerate(f, 1):
            if line_num % 10000 == 0:
                print(f"   Processed {line_num:,} lines...")

            # Check for startup line and extract git hash and build type
            if startup_pattern.search(line):
                git_hash_match = git_hash_pattern.search(line)
                if git_hash_match:
                    git_hash = git_hash_match.group(1)

                build_type_match = build_type_pattern.search(line)
                if build_type_match:
                    build_type = build_type_match.group(1)

            # Check for sync progress events
            progress_match = progress_pattern.search(line)
            if progress_match:
                timestamp_str, block_height = progress_match.groups()
                events.append(
                    {
                        "timestamp": parse_timestamp(timestamp_str),
                        "block_height": int(block_height),
                    }
                )

            # Check for sync start events
            sync_start_match = sync_start_pattern.search(line)
            if sync_start_match:
                timestamp_str, missing_blocks, duration = sync_start_match.groups()
                sync_starts.append(
                    {
                        "timestamp": parse_timestamp(timestamp_str),
                        "missing_blocks": int(missing_blocks),
                        "duration": duration,
                    }
                )

    if not events:
        print("❌ No sync events found in log file!")
        sys.exit(1)

    df = pd.DataFrame(events).sort_values("timestamp")

    # Find the last sync start
    if sync_starts:
        last_sync_start = max(sync_starts, key=lambda x: x["timestamp"])
        sync_start_time = last_sync_start["timestamp"]
        print(
            f"🚀 Found {len(sync_starts)} sync run(s), using last one starting at: {sync_start_time}"
        )
        print(
            f"📊 Last sync: {last_sync_start['missing_blocks']:,} missing blocks, took {last_sync_start['duration']} to identify"
        )

        # Filter to only include events from the last sync start onwards
        df = df[df["timestamp"] >= sync_start_time].reset_index(drop=True)
    else:
        print("⚠️  No sync start markers found, using all events")
        sync_start_time = df["timestamp"].iloc[0]

    # Calculate relative time from sync start
    df["relative_time_minutes"] = (
        df["timestamp"] - sync_start_time
    ).dt.total_seconds() / 60

    print(f"✅ Found {len(df)} sync events for analysis")
    return df, git_hash, build_type


def create_plot(
    df: pd.DataFrame,
    output_file: str,
    git_hash: str,
    build_type: str,
    show_plot: bool = False,
):
    """Create the sync progress plot."""
    print("📈 Creating plot...")

    plt.figure(figsize=(12, 8))
    plt.plot(
        df["relative_time_minutes"], df["block_height"], "b-", linewidth=1.5, alpha=0.8
    )

    # Include git hash and build type in title
    title = f"Blockchain Sync Progress (git: {git_hash}, build: {build_type})"
    plt.title(title, fontsize=16, fontweight="bold", pad=20)
    plt.xlabel("Time since sync started (minutes)", fontsize=12)
    plt.ylabel("Block Height", fontsize=12)
    plt.grid(True, alpha=0.3)

    # Add some stats as text
    total_blocks = df["block_height"].max() - df["block_height"].min()
    total_time_minutes = df["relative_time_minutes"].max()
    total_time_hours = total_time_minutes / 60
    avg_speed_per_minute = (
        total_blocks / total_time_minutes if total_time_minutes > 0 else 0
    )

    stats_text = f"Synced {total_blocks:,} blocks in {total_time_minutes:.0f} minutes ({total_time_hours:.1f} hours)\nAverage: {avg_speed_per_minute:.0f} blocks/minute"
    plt.text(
        0.02,
        0.98,
        stats_text,
        transform=plt.gca().transAxes,
        verticalalignment="top",
        fontsize=10,
        bbox=dict(boxstyle="round", facecolor="lightblue", alpha=0.8),
    )

    plt.tight_layout()

    if show_plot:
        plt.show()
    else:
        plt.savefig(output_file, dpi=300, bbox_inches="tight")
        print(f"✅ Saved plot to: {output_file}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log_file", type=Path, help="Path to the log file")
    parser.add_argument(
        "--show",
        action="store_true",
        help="Display the plot interactively (default: save and exit)",
    )

    args = parser.parse_args()

    if not args.log_file.exists():
        print(f"❌ Log file not found: {args.log_file}")
        sys.exit(1)

    # Extract and plot
    df, git_hash, build_type = extract_sync_progress(args.log_file)
    print(f"🔍 Found git hash: {git_hash}")
    print(f"🏗️  Found build type: {build_type}")

    output_dir = "sync_analysis"

    # Ensure the output directory exists
    Path(output_dir).mkdir(parents=True, exist_ok=True)

    output_file = f"{output_dir}/sync_progress_{args.log_file.stem}.png"
    create_plot(df, output_file, git_hash, build_type, args.show)

    # Export CSV with sync data
    csv_file = f"{output_dir}/sync_data_{args.log_file.stem}.csv"

    # Output every 100th row to reduce file size
    df_export = df.iloc[::100].copy()
    # Format timestamps in RFC3339 format
    df_export["timestamp"] = df_export["timestamp"].dt.strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    df_export.to_csv(csv_file, index=False)
    print(f"💾 Saved sync data to: {csv_file}")

    print(f"🎉 Done! Plotted {len(df)} sync events.")


if __name__ == "__main__":
    main()
