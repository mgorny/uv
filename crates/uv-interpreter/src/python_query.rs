//! Find a user requested python version/interpreter.

use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use tracing::{debug, instrument};

use platform_host::Platform;
use uv_cache::Cache;
use uv_fs::normalize_path;

use crate::{Error, Interpreter};

/// Find a python version/interpreter of a specific version.
///
/// Supported formats:
/// * `-p 3.10` searches for an installed Python 3.10 (`py --list-paths` on Windows, `python3.10` on
///   Linux/Mac). Specifying a patch version is not supported.
/// * `-p python3.10` or `-p python.exe` looks for a binary in `PATH`.
/// * `-p /home/ferris/.local/bin/python3.10` uses this exact Python.
///
/// When the user passes a patch version (e.g. 3.12.1), we currently search for a matching minor
/// version (e.g. `python3.12` on unix) and error when the version mismatches, as a binary with the
/// patch version (e.g. `python3.12.1`) is often not in `PATH` and we make the simplifying
/// assumption that the user has only this one patch version installed.
#[instrument(skip_all, fields(%request))]
pub fn find_requested_python(
    request: &str,
    platform: &Platform,
    cache: &Cache,
) -> Result<Option<Interpreter>, Error> {
    debug!("Starting interpreter discovery for Python @ `{request}`");
    let versions = request
        .splitn(3, '.')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>();
    if let Ok(versions) = versions {
        // `-p 3.10` or `-p 3.10.1`
        match versions.as_slice() {
            [requested_major] => find_python(
                PythonVersionSelector::Major(*requested_major),
                platform,
                cache,
            ),
            [major, minor] => find_python(
                PythonVersionSelector::MajorMinor(*major, *minor),
                platform,
                cache,
            ),
            [major, minor, requested_patch] => find_python(
                PythonVersionSelector::MajorMinorPatch(*major, *minor, *requested_patch),
                platform,
                cache,
            ),
            // SAFETY: Guaranteed by the Ok(versions) guard
            _ => unreachable!(),
        }
    } else if !request.contains(std::path::MAIN_SEPARATOR) {
        // `-p python3.10`; Generally not used on windows because all Python are `python.exe`.
        let Some(executable) = find_executable(request)? else {
            return Ok(None);
        };
        Interpreter::query(&executable, platform.clone(), cache).map(Some)
    } else {
        // `-p /home/ferris/.local/bin/python3.10`
        let executable = normalize_path(request);

        Interpreter::query(&executable, platform.clone(), cache).map(Some)
    }
}

/// Pick a sensible default for the Python a user wants when they didn't specify a version.
///
/// We prefer the test overwrite `UV_TEST_PYTHON_PATH` if it is set, otherwise `python3`/`python` or
/// `python.exe` respectively.
#[instrument(skip_all)]
pub fn find_default_python(platform: &Platform, cache: &Cache) -> Result<Interpreter, Error> {
    debug!("Starting interpreter discovery for default Python");
    try_find_default_python(platform, cache)?.ok_or(if cfg!(windows) {
        Error::NoPythonInstalledWindows
    } else if cfg!(unix) {
        Error::NoPythonInstalledUnix
    } else {
        unreachable!("Only Unix and Windows are supported")
    })
}

/// Same as [`find_default_python`] but returns `None` if no python is found instead of returning an `Err`.
pub(crate) fn try_find_default_python(
    platform: &Platform,
    cache: &Cache,
) -> Result<Option<Interpreter>, Error> {
    find_python(PythonVersionSelector::Default, platform, cache)
}

/// Finds a python version matching `selector`.
/// It searches for an existing installation in the following order:
/// * Search for the python binary in `PATH` (or `UV_TEST_PYTHON_PATH` if set). Visits each path and for each path resolves the
///   files in the following order:
///   * Major.Minor.Patch: `pythonx.y.z`, `pythonx.y`, `python.x`, `python`
///   * Major.Minor: `pythonx.y`, `pythonx`, `python`
///   * Major: `pythonx`, `python`
///   * Default: `python3`, `python`
///   * (windows): For each of the above, test for the existence of `python.bat` shim (pyenv-windows) last.
/// * (windows): Discover installations using `py --list-paths` (PEP514). Continue if `py` is not installed.
///
/// (Windows): Filter out the windows store shim (Enabled in Settings/Apps/Advanced app settings/App execution aliases).
fn find_python(
    selector: PythonVersionSelector,
    platform: &Platform,
    cache: &Cache,
) -> Result<Option<Interpreter>, Error> {
    #[allow(non_snake_case)]
    let UV_TEST_PYTHON_PATH = env::var_os("UV_TEST_PYTHON_PATH");

    let use_override = UV_TEST_PYTHON_PATH.is_some();
    let possible_names = selector.possible_names();

    #[allow(non_snake_case)]
    let PATH = UV_TEST_PYTHON_PATH
        .or(env::var_os("PATH"))
        .unwrap_or_default();

    // We use `which` here instead of joining the paths ourselves because `which` checks for us if the python
    // binary is executable and exists. It also has some extra logic that handles inconsistent casing on Windows
    // and expands `~`.
    for path in env::split_paths(&PATH) {
        for name in possible_names.iter().flatten() {
            if let Ok(paths) = which::which_in_global(&**name, Some(&path)) {
                for path in paths {
                    if cfg!(windows) && windows::is_windows_store_shim(&path) {
                        continue;
                    }

                    let interpreter = match Interpreter::query(&path, platform.clone(), cache) {
                        Ok(interpreter) => interpreter,
                        Err(Error::Python2OrOlder) => {
                            if selector.major() <= Some(2) {
                                return Err(Error::Python2OrOlder);
                            }
                            // Skip over Python 2 or older installation when querying for a recent python installation.
                            debug!("Found a Python 2 installation that isn't supported by uv, skipping.");
                            continue;
                        }
                        Err(error) => return Err(error),
                    };

                    let installation = PythonInstallation::Interpreter(interpreter);

                    if let Some(interpreter) = installation.select(selector, platform, cache)? {
                        return Ok(Some(interpreter));
                    }
                }
            }
        }

        // Python's `venv` model doesn't have this case because they use the `sys.executable` by default
        // which is sufficient to support pyenv-windows. Unfortunately, we can't rely on the executing Python version.
        // That's why we explicitly search for a Python shim as last resort.
        if cfg!(windows) {
            if let Ok(shims) = which::which_in_global("python.bat", Some(&path)) {
                for shim in shims {
                    let interpreter = match Interpreter::query(&shim, platform.clone(), cache) {
                        Ok(interpreter) => interpreter,
                        Err(error) => {
                            // Don't fail when querying the shim failed. E.g it's possible that no python version is selected
                            // in the shim in which case pyenv prints to stdout.
                            tracing::warn!("Failed to query python shim: {error}");
                            continue;
                        }
                    };

                    if let Some(interpreter) = PythonInstallation::Interpreter(interpreter)
                        .select(selector, platform, cache)?
                    {
                        return Ok(Some(interpreter));
                    }
                }
            }
        }
    }

    if cfg!(windows) && !use_override {
        // Use `py` to find the python installation on the system.
        match windows::py_list_paths() {
            Ok(paths) => {
                for entry in paths {
                    let installation = PythonInstallation::PyListPath(entry);
                    if let Some(interpreter) = installation.select(selector, platform, cache)? {
                        return Ok(Some(interpreter));
                    }
                }
            }
            Err(Error::PyList(error)) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    debug!("`py` is not installed");
                }
            }
            Err(error) => return Err(error),
        }
    }

    Ok(None)
}

/// Find the Python interpreter in `PATH` matching the given name (e.g., `python3`, respecting
/// `UV_PYTHON_PATH`.
///
/// Returns `Ok(None)` if not found.
fn find_executable<R: AsRef<OsStr> + Into<OsString> + Copy>(
    requested: R,
) -> Result<Option<PathBuf>, Error> {
    #[allow(non_snake_case)]
    let UV_TEST_PYTHON_PATH = env::var_os("UV_TEST_PYTHON_PATH");

    let use_override = UV_TEST_PYTHON_PATH.is_some();

    #[allow(non_snake_case)]
    let PATH = UV_TEST_PYTHON_PATH
        .or(env::var_os("PATH"))
        .unwrap_or_default();

    // We use `which` here instead of joining the paths ourselves because `which` checks for us if the python
    // binary is executable and exists. It also has some extra logic that handles inconsistent casing on Windows
    // and expands `~`.
    for path in env::split_paths(&PATH) {
        let paths = match which::which_in_global(requested, Some(&path)) {
            Ok(paths) => paths,
            Err(which::Error::CannotFindBinaryPath) => continue,
            Err(err) => return Err(Error::WhichError(requested.into(), err)),
        };
        for path in paths {
            if cfg!(windows) && windows::is_windows_store_shim(&path) {
                continue;
            }
            return Ok(Some(path));
        }
    }

    if cfg!(windows) && !use_override {
        // Use `py` to find the python installation on the system.
        match windows::py_list_paths() {
            Ok(paths) => {
                for entry in paths {
                    // Ex) `--python python3.12.exe`
                    if entry.executable_path.file_name() == Some(requested.as_ref()) {
                        return Ok(Some(entry.executable_path));
                    }

                    // Ex) `--python python3.12`
                    if entry
                        .executable_path
                        .file_stem()
                        .is_some_and(|stem| stem == requested.as_ref())
                    {
                        return Ok(Some(entry.executable_path));
                    }
                }
            }
            Err(Error::PyList(error)) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    debug!("`py` is not installed");
                }
            }
            Err(error) => return Err(error),
        }
    }

    Ok(None)
}

#[derive(Debug, Clone)]
struct PyListPath {
    major: u8,
    minor: u8,
    executable_path: PathBuf,
}

#[derive(Debug, Clone)]
enum PythonInstallation {
    PyListPath(PyListPath),
    Interpreter(Interpreter),
}

impl PythonInstallation {
    fn major(&self) -> u8 {
        match self {
            Self::PyListPath(PyListPath { major, .. }) => *major,
            Self::Interpreter(interpreter) => interpreter.python_major(),
        }
    }

    fn minor(&self) -> u8 {
        match self {
            Self::PyListPath(PyListPath { minor, .. }) => *minor,
            Self::Interpreter(interpreter) => interpreter.python_minor(),
        }
    }

    /// Selects the interpreter if it matches the selector (version specification).
    fn select(
        self,
        selector: PythonVersionSelector,
        platform: &Platform,
        cache: &Cache,
    ) -> Result<Option<Interpreter>, Error> {
        let selected = match selector {
            PythonVersionSelector::Default => true,

            PythonVersionSelector::Major(major) => self.major() == major,

            PythonVersionSelector::MajorMinor(major, minor) => {
                self.major() == major && self.minor() == minor
            }

            PythonVersionSelector::MajorMinorPatch(major, minor, requested_patch) => {
                let interpreter = self.into_interpreter(platform, cache)?;
                return Ok(
                    if major == interpreter.python_major()
                        && minor == interpreter.python_minor()
                        && requested_patch == interpreter.python_patch()
                    {
                        Some(interpreter)
                    } else {
                        None
                    },
                );
            }
        };

        if selected {
            self.into_interpreter(platform, cache).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(super) fn into_interpreter(
        self,
        platform: &Platform,
        cache: &Cache,
    ) -> Result<Interpreter, Error> {
        match self {
            Self::PyListPath(PyListPath {
                executable_path, ..
            }) => Interpreter::query(&executable_path, platform.clone(), cache),
            Self::Interpreter(interpreter) => Ok(interpreter),
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum PythonVersionSelector {
    Default,
    Major(u8),
    MajorMinor(u8, u8),
    MajorMinorPatch(u8, u8, u8),
}

impl PythonVersionSelector {
    fn possible_names(self) -> [Option<Cow<'static, str>>; 4] {
        let (python, python3, extension) = if cfg!(windows) {
            (
                Cow::Borrowed("python.exe"),
                Cow::Borrowed("python3.exe"),
                ".exe",
            )
        } else {
            (Cow::Borrowed("python"), Cow::Borrowed("python3"), "")
        };

        match self {
            Self::Default => [Some(python3), Some(python), None, None],
            Self::Major(major) => [
                Some(Cow::Owned(format!("python{major}{extension}"))),
                Some(python),
                None,
                None,
            ],
            Self::MajorMinor(major, minor) => [
                Some(Cow::Owned(format!("python{major}.{minor}{extension}"))),
                Some(Cow::Owned(format!("python{major}{extension}"))),
                Some(python),
                None,
            ],
            Self::MajorMinorPatch(major, minor, patch) => [
                Some(Cow::Owned(format!(
                    "python{major}.{minor}.{patch}{extension}",
                ))),
                Some(Cow::Owned(format!("python{major}.{minor}{extension}"))),
                Some(Cow::Owned(format!("python{major}{extension}"))),
                Some(python),
            ],
        }
    }

    fn major(self) -> Option<u8> {
        match self {
            Self::Default => None,
            Self::Major(major) => Some(major),
            Self::MajorMinor(major, _) => Some(major),
            Self::MajorMinorPatch(major, _, _) => Some(major),
        }
    }
}

mod windows {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use once_cell::sync::Lazy;
    use regex::Regex;
    use tracing::info_span;

    use crate::python_query::PyListPath;
    use crate::Error;

    /// ```text
    /// -V:3.12          C:\Users\Ferris\AppData\Local\Programs\Python\Python312\python.exe
    /// -V:3.8           C:\Users\Ferris\AppData\Local\Programs\Python\Python38\python.exe
    /// ```
    static PY_LIST_PATHS: Lazy<Regex> = Lazy::new(|| {
        // Without the `R` flag, paths have trailing \r
        Regex::new(r"(?mR)^ -(?:V:)?(\d).(\d+)-?(?:arm)?\d*\s*\*?\s*(.*)$").unwrap()
    });

    /// Run `py --list-paths` to find the installed pythons.
    ///
    /// The command takes 8ms on my machine.
    /// TODO(konstin): Implement <https://peps.python.org/pep-0514/> to read python installations from the registry instead.
    pub(super) fn py_list_paths() -> Result<Vec<PyListPath>, Error> {
        let output = info_span!("py_list_paths")
            .in_scope(|| Command::new("py").arg("--list-paths").output())
            .map_err(Error::PyList)?;

        // `py` sometimes prints "Installed Pythons found by py Launcher for Windows" to stderr which we ignore.
        if !output.status.success() {
            return Err(Error::PythonSubcommandOutput {
                message: format!(
                    "Running `py --list-paths` failed with status {}",
                    output.status
                ),
                exit_code: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        // Find the first python of the version we want in the list
        let stdout =
            String::from_utf8(output.stdout).map_err(|err| Error::PythonSubcommandOutput {
                message: format!("The stdout of `py --list-paths` isn't UTF-8 encoded: {err}"),
                exit_code: output.status,
                stdout: String::from_utf8_lossy(err.as_bytes()).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })?;

        Ok(PY_LIST_PATHS
            .captures_iter(&stdout)
            .filter_map(|captures| {
                let (_, [major, minor, path]) = captures.extract();
                if let (Some(major), Some(minor)) =
                    (major.parse::<u8>().ok(), minor.parse::<u8>().ok())
                {
                    Some(PyListPath {
                        major,
                        minor,
                        executable_path: PathBuf::from(path),
                    })
                } else {
                    None
                }
            })
            .collect())
    }

    /// On Windows we might encounter the windows store proxy shim (Enabled in Settings/Apps/Advanced app settings/App execution aliases).
    /// This requires quite a bit of custom logic to figure out what this thing does.
    ///
    /// This is a pretty dumb way.  We know how to parse this reparse point, but Microsoft
    /// does not want us to do this as the format is unstable.  So this is a best effort way.
    /// we just hope that the reparse point has the python redirector in it, when it's not
    /// pointing to a valid Python.
    ///
    /// Matches against paths like:
    ///     `C:\Users\crmar\AppData\Local\Microsoft\WindowsApps\python.exe`
    pub(super) fn is_windows_store_shim(path: &Path) -> bool {
        // Rye uses a more sophisticated test to identify the windows store shim.
        // Unfortunately, it only works with the `python.exe` shim but not `python3.exe`.
        // What we do here is a very naive implementation but probably sufficient for all we need.
        // There's the risk of false positives but I consider it rare, considering how specific
        // the path is.
        if !path.is_absolute() {
            return false;
        }

        let mut components = path.components().rev();

        // Ex) `python.exe` or `python3.exe` or `python3.12.exe`
        if !components
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .and_then(|component| component.rsplit_once('.'))
            .is_some_and(|(name, extension)| name.starts_with("python") && extension == "exe")
        {
            return false;
        }

        // Ex) `WindowsApps`
        if !components
            .next()
            .is_some_and(|component| component.as_os_str() == "WindowsApps")
        {
            return false;
        }

        // Ex) `Microsoft`
        if !components
            .next()
            .is_some_and(|component| component.as_os_str() == "Microsoft")
        {
            return false;
        }

        // Ex) `Local`
        if !components
            .next()
            .is_some_and(|component| component.as_os_str() == "Local")
        {
            return false;
        }

        true
    }

    #[cfg(test)]
    #[cfg(windows)]
    mod tests {
        use std::fmt::Debug;

        use insta::assert_snapshot;
        use itertools::Itertools;

        use platform_host::Platform;
        use uv_cache::Cache;

        use crate::{find_requested_python, Error};

        fn format_err<T: Debug>(err: Result<T, Error>) -> String {
            anyhow::Error::new(err.unwrap_err())
                .chain()
                .join("\n  Caused by: ")
        }

        #[test]
        fn no_such_python_path() {
            let result = find_requested_python(
                r"C:\does\not\exists\python3.12",
                &Platform::current().unwrap(),
                &Cache::temp().unwrap(),
            );
            insta::with_settings!({
                filters => vec![
                    // The exact message is host language dependent
                    (r"Caused by: .* \(os error 3\)", "Caused by: The system cannot find the path specified. (os error 3)")
                ]
            }, {
                assert_snapshot!(
                    format_err(result), @r###"
        failed to canonicalize path `C:\does\not\exists\python3.12`
          Caused by: The system cannot find the path specified. (os error 3)
        "###);
            });
        }

        #[test]
        fn detect_shim() {
            assert!(super::is_windows_store_shim(
                r"C:\Users\crmar\AppData\Local\Microsoft\WindowsApps\python.exe".as_ref()
            ));
            assert!(super::is_windows_store_shim(
                r"C:\Users\crmar\AppData\Local\Microsoft\WindowsApps\python3.exe".as_ref()
            ));
            assert!(super::is_windows_store_shim(
                r"C:\Users\crmar\AppData\Local\Microsoft\WindowsApps\python3.12.exe".as_ref()
            ));
            assert!(!super::is_windows_store_shim(
                r"C:\Users\crmar\AppData\Local\Microsoft\WindowsApps\PythonSoftwareFoundation.Python.3.11_qbs5n2kfra8p0\python.exe".as_ref()
            ));
        }
    }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use itertools::Itertools;

    use platform_host::Platform;
    use uv_cache::Cache;

    use crate::python_query::find_requested_python;
    use crate::Error;

    fn format_err<T: std::fmt::Debug>(err: Result<T, Error>) -> String {
        anyhow::Error::new(err.unwrap_err())
            .chain()
            .join("\n  Caused by: ")
    }

    #[test]
    fn no_such_python_version() {
        let request = "3.1000";
        let result = find_requested_python(
            request,
            &Platform::current().unwrap(),
            &Cache::temp().unwrap(),
        )
        .unwrap()
        .ok_or(Error::NoSuchPython(request.to_string()));
        assert_snapshot!(
            format_err(result),
            @"No Python 3.1000 In `PATH`. Is Python 3.1000 installed?"
        );
    }

    #[test]
    fn no_such_python_binary() {
        let request = "python3.1000";
        let result = find_requested_python(
            request,
            &Platform::current().unwrap(),
            &Cache::temp().unwrap(),
        )
        .unwrap()
        .ok_or(Error::NoSuchPython(request.to_string()));
        assert_snapshot!(
            format_err(result),
            @"No Python python3.1000 In `PATH`. Is Python python3.1000 installed?"
        );
    }

    #[test]
    fn no_such_python_path() {
        let result = find_requested_python(
            "/does/not/exists/python3.12",
            &Platform::current().unwrap(),
            &Cache::temp().unwrap(),
        );
        assert_snapshot!(
            format_err(result), @r###"
        failed to canonicalize path `/does/not/exists/python3.12`
          Caused by: No such file or directory (os error 2)
        "###);
    }
}
