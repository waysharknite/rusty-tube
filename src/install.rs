//! Глобальная установка: копия программы в постоянную папку из PATH, чтобы
//! `rusty-tube` запускался из любой папки терминала.
//!
//! - Windows: `%LOCALAPPDATA%\Programs\rusty-tube\rusty-tube.exe`, папка добавляется
//!   в PATH пользователя (реестр `HKCU\Environment`), права администратора не нужны.
//! - macOS (и Linux): первая доступная для записи папка из PATH (`bin` Homebrew,
//!   `/usr/local/bin`, `~/.local/bin`, `~/bin`). Если такой нет — `~/.local/bin`
//!   и строка `export PATH=…` в профиле shell.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(windows)]
const EXE: &str = "rusty-tube.exe";
#[cfg(not(windows))]
const EXE: &str = "rusty-tube";

/// Куда и как будет установлена программа.
pub struct Plan {
    pub dir: PathBuf,
    /// Папку нужно добавить в PATH (иначе она там уже есть).
    pub add_to_path: bool,
}

impl Plan {
    pub fn target(&self) -> PathBuf {
        self.dir.join(EXE)
    }

    /// Описание изменений для окна подтверждения.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![
            "Программа будет скопирована в папку:".to_string(),
            format!("  {}", self.dir.display()),
        ];
        if self.add_to_path {
            #[cfg(windows)]
            lines.push("и эта папка добавится в PATH пользователя.".into());
            #[cfg(not(windows))]
            lines.push(format!(
                "и эта папка добавится в PATH в {}.",
                display_home(&profile_file())
            ));
        }
        lines
    }
}

/// Установлена ли программа глобально.
pub fn is_installed() -> bool {
    let plan = plan();
    plan.target().is_file() && !plan.add_to_path
}

/// Копирует программу и при необходимости добавляет папку в PATH.
/// Возвращает сообщение для пользователя.
pub fn install() -> Result<String, String> {
    let plan = plan();
    let current = env::current_exe().map_err(|e| format!("Не найден файл программы: {e}"))?;
    let target = plan.target();
    if !same_file(&current, &target) {
        fs::create_dir_all(&plan.dir).map_err(|e| format!("Не удалось создать папку: {e}"))?;
        copy_exe(&current, &target)?;
    }
    if plan.add_to_path {
        add_to_path(&plan.dir)?;
    }
    Ok("Готово. Откройте новое окно терминала и наберите rusty-tube".into())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
pub fn plan() -> Plan {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Programs")
        .join("rusty-tube");
    let add_to_path = !user_path::read()
        .map(|(path, _)| path_contains(&path, &dir))
        .unwrap_or(false);
    Plan { dir, add_to_path }
}

/// Запущенный exe нельзя перезаписать, но можно переименовать: старая копия
/// уезжает в `.old` и удаляется при следующей установке.
#[cfg(windows)]
fn copy_exe(from: &Path, to: &Path) -> Result<(), String> {
    let old = to.with_extension("exe.old");
    let _ = fs::remove_file(&old);
    if to.exists() {
        fs::rename(to, &old).map_err(|e| format!("Не удалось заменить старую копию: {e}"))?;
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("Не удалось скопировать программу: {e}"))
}

/// Есть ли папка в PATH (без учёта регистра, завершающей `\` и переменных `%…%`).
#[cfg(windows)]
fn path_contains(path: &str, dir: &Path) -> bool {
    let normalize = |s: &str| {
        let expanded = expand_vars(s.trim().trim_matches('"'));
        expanded.trim_end_matches(['\\', '/']).to_lowercase()
    };
    let dir = normalize(&dir.to_string_lossy());
    path.split(';').any(|entry| normalize(entry) == dir)
}

#[cfg(windows)]
fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match env::var(name) {
                    Ok(value) if !name.is_empty() => out.push_str(&value),
                    _ => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(windows)]
fn add_to_path(dir: &Path) -> Result<(), String> {
    let (path, kind) = user_path::read()?;
    let mut entries: Vec<&str> = path.split(';').filter(|e| !e.trim().is_empty()).collect();
    let dir = dir.to_string_lossy();
    entries.push(&dir);
    user_path::write(&entries.join(";"), kind)?;
    user_path::broadcast();
    Ok(())
}

/// PATH пользователя в реестре. Тип значения сохраняем: обычно это REG_EXPAND_SZ
/// с переменными вроде `%USERPROFILE%`, и переписывать его как REG_SZ нельзя.
#[cfg(windows)]
mod user_path {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
            REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    };

    const ERROR_FILE_NOT_FOUND: u32 = 2;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open() -> Result<Key, String> {
        let mut hkey: HKEY = ptr::null_mut();
        let subkey = wide("Environment");
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut hkey,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("Нет доступа к переменным среды (код {status})"));
        }
        Ok(Key(hkey))
    }

    /// Значение и его тип; если PATH пользователя ещё нет — пустая строка.
    pub fn read() -> Result<(String, REG_VALUE_TYPE), String> {
        let key = open()?;
        let name = wide("Path");
        let mut kind: REG_VALUE_TYPE = 0;
        let mut size: u32 = 0;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok((String::new(), REG_EXPAND_SZ));
        }
        if status != ERROR_SUCCESS {
            return Err(format!("Не удалось прочитать PATH (код {status})"));
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("Не удалось прочитать PATH (код {status})"));
        }
        buf.truncate(size as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            return Err("PATH пользователя в реестре имеет неожиданный тип".into());
        }
        Ok((String::from_utf16_lossy(&buf), kind))
    }

    pub fn write(value: &str, kind: REG_VALUE_TYPE) -> Result<(), String> {
        let key = open()?;
        let name = wide("Path");
        let data = wide(value);
        let status = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                kind,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("Не удалось изменить PATH (код {status})"));
        }
        Ok(())
    }

    /// Сообщаем Проводнику, что переменные среды изменились: новые окна
    /// терминала увидят PATH без выхода из системы.
    pub fn broadcast() {
        let param = wide("Environment");
        let mut result = 0usize;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                param.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                2000,
                &mut result,
            );
        }
    }
}

// ------------------------------------------------------------------ macOS / Linux

#[cfg(not(windows))]
pub fn plan() -> Plan {
    let in_path: Vec<PathBuf> = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();
    // Уже установленная копия в одной из папок PATH — обновляем её.
    if let Some(dir) = candidates()
        .into_iter()
        .find(|d| in_path.contains(d) && d.join(EXE).is_file() && writable(d))
    {
        return Plan {
            dir,
            add_to_path: false,
        };
    }
    if let Some(dir) = candidates()
        .into_iter()
        .find(|d| in_path.contains(d) && d.is_dir() && writable(d))
    {
        return Plan {
            dir,
            add_to_path: false,
        };
    }
    let dir = local_bin();
    let add_to_path = !profile_has(&dir);
    Plan { dir, add_to_path }
}

/// Папки в порядке предпочтения. `bin` Homebrew принадлежит пользователю,
/// поэтому sudo не нужен.
#[cfg(not(windows))]
fn candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(prefix) = env::var_os("HOMEBREW_PREFIX").filter(|p| !p.is_empty()) {
        dirs.push(PathBuf::from(prefix).join("bin"));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.push(local_bin());
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("bin"));
    }
    dirs
}

#[cfg(not(windows))]
fn local_bin() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/bin")
}

#[cfg(not(windows))]
fn writable(dir: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    CString::new(dir.as_os_str().as_bytes())
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// Файл, который читает login-shell: zsh — `~/.zprofile`, иначе
/// `~/.bash_profile` на macOS и `~/.profile` в Linux.
#[cfg(not(windows))]
fn profile_file() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let shell = env::var("SHELL").unwrap_or_default();
    if shell.ends_with("zsh") {
        home.join(".zprofile")
    } else if cfg!(target_os = "macos") {
        home.join(".bash_profile")
    } else {
        home.join(".profile")
    }
}

#[cfg(not(windows))]
fn display_home(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

#[cfg(not(windows))]
const PROFILE_LINE: &str = r#"export PATH="$HOME/.local/bin:$PATH""#;

#[cfg(not(windows))]
fn profile_has(dir: &Path) -> bool {
    let in_env = env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| d == dir));
    in_env
        || fs::read_to_string(profile_file())
            .is_ok_and(|text| text.lines().any(|l| l.trim() == PROFILE_LINE))
}

/// Запись во временный файл и переименование: так можно заменить и запущенную копию.
#[cfg(not(windows))]
fn copy_exe(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = to.with_extension("tmp");
    let result = fs::copy(from, &tmp)
        .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)))
        .and_then(|_| fs::rename(&tmp, to));
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Не удалось скопировать программу: {e}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn add_to_path(_dir: &Path) -> Result<(), String> {
    use std::io::Write;
    let profile = profile_file();
    let existing = fs::read_to_string(&profile).unwrap_or_default();
    let mut text = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n# rusty-tube\n");
    text.push_str(PROFILE_LINE);
    text.push('\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&profile)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("Не удалось изменить {}: {e}", display_home(&profile)))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn finds_dir_in_path() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Programs\rusty-tube");
        assert!(path_contains(
            r"C:\a;c:\users\ME\appdata\local\programs\rusty-tube\;C:\b",
            dir
        ));
        assert!(!path_contains(r"C:\a;C:\b", dir));
        assert!(!path_contains("", dir));
    }

    #[test]
    fn reads_user_path() {
        let (_, kind) = user_path::read().unwrap();
        use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ};
        assert!(kind == REG_SZ || kind == REG_EXPAND_SZ);
    }

    #[test]
    fn expands_vars() {
        // SAFETY: тест однопоточно задаёт свою переменную.
        unsafe { env::set_var("RUSTY_TUBE_TEST_VAR", "X") };
        assert_eq!(expand_vars(r"%RUSTY_TUBE_TEST_VAR%\bin"), r"X\bin");
        assert_eq!(expand_vars("%NO_SUCH_VAR_RT%"), "%NO_SUCH_VAR_RT%");
        assert_eq!(expand_vars("50%"), "50%");
    }
}
