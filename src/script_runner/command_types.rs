use std::thread;

use hashbrown::HashMap;
use serde::Deserialize;

use super::command::Command;

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum CommandTypes {
    /// A command defined in the script.
    Command(Command),
    /// A set of commands that should be executed together.
    Commands(Vec<Command>),
}

impl CommandTypes {
    pub async fn execute(
        &self,
        context: &mut HashMap<String, String>,
    ) -> Result<Option<String>, String> {
        match self {
            CommandTypes::Command(cmd) => cmd.clone().execute(context).await,
            CommandTypes::Commands(cmds) => {
                let num_cmds = cmds.len();
                if num_cmds == 0 {
                    return Ok(None);
                }

                // Determine number of threads to create
                let num_threads = if num_cmds < 4 { num_cmds } else { 4 };
                let mut handles = Vec::new();
                let mut results = Vec::with_capacity(num_cmds);

                // Group commands for thread allocation
                let mut command_groups: Vec<Vec<Command>> = Vec::with_capacity(num_threads);
                for _ in 0..num_threads {
                    command_groups.push(Vec::new());
                }

                // Distribute commands across groups
                for (i, cmd) in cmds.iter().enumerate() {
                    let group_index = i % num_threads;
                    command_groups[group_index].push(cmd.clone());
                }

                // Spawn threads for each group
                for cmd_group in command_groups {
                    if cmd_group.is_empty() {
                        continue;
                    }

                    let context_clone = context.clone();

                    handles.push(thread::spawn(move || {
                        let mut group_results = Vec::new();
                        let mut local_context = context_clone;

                        for mut cmd in cmd_group {
                            match futures::executor::block_on(cmd.execute(&mut local_context)) {
                                Ok(output) => group_results.push(output),
                                Err(e) => return Err(format!("Command execution failed: {e}")),
                            }
                        }

                        Ok(group_results)
                    }));
                }

                // Collect results from all threads
                for handle in handles {
                    match handle.join() {
                        Ok(group_result) => match group_result {
                            Ok(outputs) => results.extend(outputs),
                            Err(e) => return Err(e),
                        },
                        Err(_) => {
                            return Err("Thread panicked during command execution".to_string());
                        }
                    }
                }

                // Combine all outputs
                let combined_results: Vec<String> = results.into_iter().flatten().collect();

                Ok(Some(combined_results.join(",")))
            }
        }
    }
}
