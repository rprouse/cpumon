//! `cpumon`: monitor Windows CPU/GPU usage and record the processes responsible.

fn main() -> anyhow::Result<()> {
    cpumon::run()
}
