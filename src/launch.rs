use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::Serialize;
use thiserror::Error;

use crate::paths::PortableLayout;
use crate::readiness::BackendRoute;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum HostPlatform {
    Windows,
    Linux,
    Unsupported,
}

impl HostPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unsupported
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<PathBuf>,
    pub current_dir: PathBuf,
    pub env: BTreeMap<String, PathBuf>,
    pub generated_files: Vec<(PathBuf, String)>,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("this host platform is not supported")]
    Unsupported,
    #[error("cannot determine a local data directory for the Wine prefix")]
    NoDataDirectory,
    #[error("game system name is invalid: {0}")]
    InvalidSystem(String),
    #[error("Wine cannot address a relative game path: {0}")]
    RelativeWinePath(PathBuf),
    #[error("Wine cannot address a non-Unicode game path: {0}")]
    NonUnicodeWinePath(PathBuf),
    #[error("failed to launch RetroBat: {0}")]
    Io(#[from] io::Error),
}

impl LaunchPlan {
    pub fn for_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
    ) -> Result<Self, LaunchError> {
        match host {
            HostPlatform::Windows => Ok(Self {
                program: layout.retrobat_executable(),
                args: Vec::new(),
                current_dir: layout.retrobat_root(),
                env: BTreeMap::new(),
                generated_files: Vec::new(),
            }),
            HostPlatform::Linux => {
                let data_dir = linux_data_dir.ok_or(LaunchError::NoDataDirectory)?;
                Ok(Self {
                    program: PathBuf::from("wine"),
                    args: vec![layout.retrobat_executable()],
                    current_dir: layout.retrobat_root(),
                    env: BTreeMap::from([(
                        "WINEPREFIX".to_owned(),
                        data_dir.join("retrobat-portable").join("wine-prefix"),
                    )]),
                    generated_files: Vec::new(),
                })
            }
            HostPlatform::Unsupported => Err(LaunchError::Unsupported),
        }
    }

    pub fn for_current_host(layout: &PortableLayout) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_host(layout, HostPlatform::current(), data.as_deref())
    }

    pub fn for_game_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        Self::for_game_host_with_backend(layout, host, linux_data_dir, system, rom, None)
    }

    pub fn for_game_host_with_backend(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        rom: &Path,
        backend: Option<&BackendRoute>,
    ) -> Result<Self, LaunchError> {
        if system.is_empty()
            || !system.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            })
        {
            return Err(LaunchError::InvalidSystem(system.to_owned()));
        }
        if system == "chip8" {
            return Self::for_retroarch_core(layout, host, linux_data_dir, system, "jaxe", rom);
        }
        if host == HostPlatform::Linux
            && let Some(BackendRoute {
                emulator,
                core: Some(core),
                ..
            }) = backend
            && emulator.eq_ignore_ascii_case("libretro")
        {
            // EmulatorLauncher is a .NET Framework/WinForms program. Several
            // current Wine-Mono combinations abort before spawning RetroArch
            // (gmisc-win32 filename assertion). Libretro has a complete,
            // stable direct command line, so Linux card launches bypass that
            // unnecessary process without bypassing the selected core.
            return Self::for_retroarch_core(layout, host, linux_data_dir, system, core, rom);
        }
        if host == HostPlatform::Linux
            && backend.is_some_and(|route| route.emulator.eq_ignore_ascii_case("windows"))
            && rom
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            let data_dir = linux_data_dir.ok_or(LaunchError::NoDataDirectory)?;
            return Ok(Self {
                program: PathBuf::from("wine"),
                args: vec![rom.to_owned()],
                current_dir: rom.parent().unwrap_or_else(|| Path::new(".")).to_owned(),
                env: BTreeMap::from([(
                    "WINEPREFIX".to_owned(),
                    data_dir.join("retrobat-portable").join("wine-prefix"),
                )]),
                generated_files: Vec::new(),
            });
        }
        if host == HostPlatform::Linux
            && let Some(backend) = backend
            && let Some(plan) = native_linux_game_plan(layout, backend, rom)
        {
            return Ok(plan);
        }

        let launcher = layout.emulator_launcher_executable();
        let mut args = vec![PathBuf::from("-system"), PathBuf::from(system)];
        if let Some(backend) = backend {
            args.extend([PathBuf::from("-emulator"), PathBuf::from(&backend.emulator)]);
            if let Some(core) = &backend.core {
                args.extend([PathBuf::from("-core"), PathBuf::from(core)]);
            }
        }
        args.push(PathBuf::from("-rom"));
        let (program, current_dir, env) = match host {
            HostPlatform::Windows => {
                args.push(rom.to_owned());
                (launcher, layout.emulationstation_root(), BTreeMap::new())
            }
            HostPlatform::Linux => {
                let data_dir = linux_data_dir.ok_or(LaunchError::NoDataDirectory)?;
                args.insert(0, launcher);
                args.push(wine_path(rom)?);
                (
                    PathBuf::from("wine"),
                    layout.emulationstation_root(),
                    BTreeMap::from([(
                        "WINEPREFIX".to_owned(),
                        data_dir.join("retrobat-portable").join("wine-prefix"),
                    )]),
                )
            }
            HostPlatform::Unsupported => return Err(LaunchError::Unsupported),
        };
        Ok(Self {
            program,
            args,
            current_dir,
            env,
            generated_files: Vec::new(),
        })
    }

    fn for_retroarch_core(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        system: &str,
        core: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        let retroarch = layout.retroarch_executable();
        let core = layout.retroarch_core(core);
        let save = layout.retrobat_root().join("saves").join(system);
        let state = layout
            .retrobat_root()
            .join("saves")
            .join(system)
            .join("states");
        let append_config = layout
            .metadata_root()
            .join("runtime")
            .join("retroarch")
            .join(format!("{system}.cfg"));
        let mut args = Vec::new();
        let (program, env, config_argument, save_value, state_value) = match host {
            HostPlatform::Windows => {
                let save_value = retroarch_config_path(&save);
                let state_value = retroarch_config_path(&state);
                args.extend([
                    PathBuf::from("--appendconfig"),
                    append_config.clone(),
                    PathBuf::from("-L"),
                    core,
                    rom.to_owned(),
                ]);
                (
                    retroarch,
                    BTreeMap::new(),
                    append_config.clone(),
                    save_value,
                    state_value,
                )
            }
            HostPlatform::Linux => {
                let data_dir = linux_data_dir.ok_or(LaunchError::NoDataDirectory)?;
                let config_argument = wine_path(&append_config)?;
                let save_value = retroarch_config_path(&wine_path(&save)?);
                let state_value = retroarch_config_path(&wine_path(&state)?);
                args.extend([
                    retroarch,
                    PathBuf::from("--appendconfig"),
                    config_argument.clone(),
                    PathBuf::from("-L"),
                    wine_path(&core)?,
                    wine_path(rom)?,
                ]);
                (
                    PathBuf::from("wine"),
                    BTreeMap::from([(
                        "WINEPREFIX".to_owned(),
                        data_dir.join("retrobat-portable").join("wine-prefix"),
                    )]),
                    config_argument,
                    save_value,
                    state_value,
                )
            }
            HostPlatform::Unsupported => return Err(LaunchError::Unsupported),
        };
        debug_assert!(args.iter().any(|argument| argument == &config_argument));
        let mut config = format!(
            concat!(
                "savefile_directory = \"{}\"\n",
                "savestate_directory = \"{}\"\n",
                "config_save_on_exit = \"false\"\n",
                "audio_enable = \"true\"\n",
                "audio_driver = \"xaudio\"\n",
                "audio_mute_enable = \"false\"\n",
                "audio_mixer_mute_enable = \"false\"\n",
                "audio_volume = \"0.000000\"\n",
                "input_autodetect_enable = \"true\"\n",
                "input_joypad_driver = \"sdl2\"\n",
                "input_player1_joypad_index = \"0\"\n",
                "input_player1_analog_dpad_mode = \"1\"\n",
                "input_player1_b_btn = \"0\"\n",
                "input_player1_a_btn = \"1\"\n",
                "input_player1_y_btn = \"2\"\n",
                "input_player1_x_btn = \"3\"\n",
                "input_player1_select_btn = \"4\"\n",
                "input_player1_start_btn = \"6\"\n",
                "input_player1_up_btn = \"11\"\n",
                "input_player1_down_btn = \"12\"\n",
                "input_player1_left_btn = \"13\"\n",
                "input_player1_right_btn = \"14\"\n"
            ),
            save_value.replace('"', "\\\""),
            state_value.replace('"', "\\\"")
        );
        if system == "mame" {
            config.push_str(concat!(
                "input_player1_select = \"num5\"\n",
                "input_player1_start = \"num1\"\n",
                "input_player1_up = \"up\"\n",
                "input_player1_down = \"down\"\n",
                "input_player1_left = \"left\"\n",
                "input_player1_right = \"right\"\n"
            ));
        }
        Ok(Self {
            program,
            args,
            current_dir: layout.retroarch_root(),
            env,
            generated_files: vec![(append_config, config)],
        })
    }

    pub fn for_current_game(
        layout: &PortableLayout,
        system: &str,
        rom: &Path,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_game_host(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            system,
            rom,
        )
    }

    pub fn for_current_game_with_backend(
        layout: &PortableLayout,
        system: &str,
        rom: &Path,
        backend: Option<&BackendRoute>,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_game_host_with_backend(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            system,
            rom,
            backend,
        )
    }

    pub fn for_rpcs3_firmware_install_host(
        layout: &PortableLayout,
        host: HostPlatform,
        linux_data_dir: Option<&Path>,
        firmware: &Path,
    ) -> Result<Self, LaunchError> {
        let rpcs3 = layout.rpcs3_executable();
        let current_dir = layout.emulator_root("rpcs3");
        let native = layout.linux_runtime_root().join("RPCS3.AppImage");
        let (program, args, env) = match host {
            HostPlatform::Windows => (
                rpcs3,
                vec![PathBuf::from("--installfw"), firmware.to_owned()],
                BTreeMap::new(),
            ),
            HostPlatform::Linux => {
                if native.is_file() {
                    return Ok(Self {
                        program: native,
                        args: vec![PathBuf::from("--installfw"), firmware.to_owned()],
                        current_dir: layout.linux_runtime_root(),
                        env: native_linux_environment(layout, "rpcs3"),
                        generated_files: Vec::new(),
                    });
                }
                let data_dir = linux_data_dir.ok_or(LaunchError::NoDataDirectory)?;
                (
                    PathBuf::from("wine"),
                    vec![rpcs3, PathBuf::from("--installfw"), wine_path(firmware)?],
                    BTreeMap::from([(
                        "WINEPREFIX".to_owned(),
                        data_dir.join("retrobat-portable").join("wine-prefix"),
                    )]),
                )
            }
            HostPlatform::Unsupported => return Err(LaunchError::Unsupported),
        };
        Ok(Self {
            program,
            args,
            current_dir,
            env,
            generated_files: Vec::new(),
        })
    }

    pub fn for_current_rpcs3_firmware_install(
        layout: &PortableLayout,
        firmware: &Path,
    ) -> Result<Self, LaunchError> {
        let data = dirs::data_local_dir();
        Self::for_rpcs3_firmware_install_host(
            layout,
            HostPlatform::current(),
            data.as_deref(),
            firmware,
        )
    }

    pub fn spawn(&self) -> Result<Child, LaunchError> {
        self.prepare_runtime()?;

        let mut command = Command::new(&self.program);
        command.args(&self.args).current_dir(&self.current_dir);
        #[cfg(unix)]
        command.process_group(0);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        Ok(command.spawn()?)
    }

    fn prepare_runtime(&self) -> Result<(), LaunchError> {
        if let Some(prefix) = self.env.get("WINEPREFIX") {
            fs::create_dir_all(prefix)?;
        }
        for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
            if let Some(directory) = self.env.get(key) {
                fs::create_dir_all(directory)?;
            }
        }
        for (path, contents) in &self.generated_files {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, contents)?;
        }
        Ok(())
    }
}

pub fn terminate_process_tree(child: &mut Child, force: bool) -> Result<(), LaunchError> {
    terminate_process_tree_id(child.id(), force)
}

pub fn terminate_process_tree_id(process_id: u32, force: bool) -> Result<(), LaunchError> {
    #[cfg(unix)]
    {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let process_group = -(process_id as i32);
        // SAFETY: kill is called with a process-group id created by
        // CommandExt::process_group(0). No pointers cross the FFI boundary.
        let result = unsafe { libc::kill(process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(LaunchError::Io(error))
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &process_id.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(LaunchError::Io(io::Error::other(format!(
                "taskkill exited with {status}"
            ))))
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = force;
        Err(LaunchError::Unsupported)
    }
}

pub fn process_tree_is_running(process_id: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 performs existence/permission checking only and
        // receives no pointer arguments.
        (unsafe { libc::kill(-(process_id as i32), 0) } == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = process_id;
        false
    }
}

fn retroarch_config_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn native_linux_game_plan(
    layout: &PortableLayout,
    backend: &BackendRoute,
    rom: &Path,
) -> Option<LaunchPlan> {
    let runtime = layout.linux_runtime_root();
    let emulator = backend.emulator.to_ascii_lowercase();
    let (program, args, key) = match emulator.as_str() {
        "eden" => (
            runtime.join("Eden.AppImage"),
            vec![PathBuf::from("-f"), PathBuf::from("-g"), rom.to_owned()],
            "eden",
        ),
        "cemu" => (
            runtime.join("Cemu.AppImage"),
            vec![PathBuf::from("-g"), rom.to_owned(), PathBuf::from("-f")],
            "cemu",
        ),
        "rpcs3" => (
            runtime.join("RPCS3.AppImage"),
            vec![PathBuf::from("--no-gui"), rom.to_owned()],
            "rpcs3",
        ),
        "shadps4" => (
            runtime.join("shadPS4/Shadps4-sdl.AppImage"),
            vec![rom.to_owned()],
            "shadps4",
        ),
        "xenia-canary" => (
            runtime.join("XeniaCanary.AppImage"),
            vec![rom.to_owned()],
            "xenia-canary",
        ),
        _ => return None,
    };
    program.is_file().then(|| LaunchPlan {
        program,
        args,
        current_dir: runtime,
        env: native_linux_environment(layout, key),
        generated_files: Vec::new(),
    })
}

fn native_linux_environment(layout: &PortableLayout, emulator: &str) -> BTreeMap<String, PathBuf> {
    let root = layout
        .metadata_root()
        .join("runtime")
        .join("linux")
        .join(emulator);
    BTreeMap::from([
        ("XDG_CONFIG_HOME".to_owned(), root.join("config")),
        ("XDG_DATA_HOME".to_owned(), root.join("data")),
        ("XDG_CACHE_HOME".to_owned(), root.join("cache")),
    ])
}

fn wine_path(path: &Path) -> Result<PathBuf, LaunchError> {
    if !path.is_absolute() {
        return Err(LaunchError::RelativeWinePath(path.to_owned()));
    }
    let Some(path_text) = path.to_str() else {
        return Err(LaunchError::NonUnicodeWinePath(path.to_owned()));
    };
    Ok(PathBuf::from(format!("Z:{}", path_text.replace('/', "\\"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_launch_rejects_a_system_that_could_be_parsed_as_arguments() {
        let layout = PortableLayout::new("/opt/retroport");
        let result = LaunchPlan::for_game_host(
            &layout,
            HostPlatform::Linux,
            Some(Path::new("/home/user/.local/share")),
            "../gb",
            Path::new("/tmp/game.gb"),
        );
        assert!(matches!(result, Err(LaunchError::InvalidSystem(_))));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_kills_the_entire_spawned_process_group() {
        let mut child = LaunchPlan {
            program: PathBuf::from("sh"),
            args: vec![PathBuf::from("-c"), PathBuf::from("sleep 30 & wait")],
            current_dir: PathBuf::from("/tmp"),
            env: BTreeMap::new(),
            generated_files: Vec::new(),
        }
        .spawn()
        .unwrap();
        let process_id = child.id();
        assert!(process_tree_is_running(process_id));

        terminate_process_tree(&mut child, false).unwrap();
        for _ in 0..100 {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(child.try_wait().unwrap().is_some());
        assert!(!process_tree_is_running(process_id));
    }
}
