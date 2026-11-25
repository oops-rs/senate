import Darwin
import Foundation
import Testing
@testable import Senate

/// A scene's manifest, read strictly: what is accepted and what is refused.
@Suite struct SceneManifestTests {
    func parse(_ text: String) -> Result<SceneManifest, SceneManifest.Problem> {
        Result { () throws(SceneManifest.Problem) in try SceneManifest.parse(Data(text.utf8)) }
    }

    @Test func theFixtureIsAccepted() throws {
        let m = try SceneManifest.parse(SceneFixture.manifest)
        #expect(m.name == "Fixture" && m.version == "1.0.0" && m.entry == "index.html")
        #expect(m.required == ["state_patch"] && m.optional == ["app_links"] && m.cardWrites == ["fixture.badge/v1"])
        #expect(!m.wantsTopology)
    }

    @Test func accepted() throws {
        let table: [[String: Any?]] = [
            [:],
            ["version": "12.0.3-beta.2"],
            ["entry": "app/main/index.html"],
            ["name": "小镇 Town ✦"],
            ["observe": ["major": 1, "required": [], "optional": ["topology"]]],
            ["cards": ["reads": ["town.look/v1", "a.b.c.d/v999"], "writes": []]],
        ]
        for changes in table {
            let text = SceneFixture.manifest(changes)
            #expect((try? parse(text).get()) != nil, "\(changes)")
        }
        #expect(try parse(SceneFixture.manifest(["observe": ["major": 1, "required": [], "optional": ["topology"]]])).get().wantsTopology)
    }

    @Test func refused() {
        let bad: [(String, [String: Any?])] = [
            ("missing name", ["name": nil]),
            ("missing cards", ["cards": nil]),
            ("unknown field", ["description": "x"]),
            ("empty name", ["name": ""]),
            ("long name", ["name": String(repeating: "x", count: 65)]),
            ("spaced name", ["name": " Town"]),
            ("control in name", ["name": "Town\u{7}"]),
            ("bidi override in name", ["name": "Town\u{202E}nwoT"]),
            ("tag character in name", ["name": "Town\u{E0041}"]),
            ("name not a string", ["name": 5]),
            ("short version", ["version": "1.0"]),
            ("leading zero version", ["version": "01.0.0"]),
            ("version with build", ["version": "1.0.0+5"]),
            ("entry escapes", ["entry": "../index.html"]),
            ("absolute entry", ["entry": "/index.html"]),
            ("entry not html", ["entry": "index.js"]),
            ("empty segment", ["entry": "a//index.html"]),
            ("dot segment", ["entry": "./index.html"]),
            ("hidden entry", ["entry": ".index.html"]),
            ("backslash entry", ["entry": "a\\index.html"]),
            ("escaped entry", ["entry": "%2e%2e/index.html"]),
            ("non-ASCII entry", ["entry": "índex.html"]),
            ("observe major 2", ["observe": ["major": 2, "required": [], "optional": []]]),
            ("observe major string", ["observe": ["major": "1", "required": [], "optional": []]]),
            ("observe major boolean", ["observe": ["major": true, "required": [], "optional": []]]),
            ("observe major fraction", ["observe": ["major": 1.5, "required": [], "optional": []]]),
            ("observe missing optional", ["observe": ["major": 1, "required": []]]),
            ("observe extra field", ["observe": ["major": 1, "required": [], "optional": [], "scope": "lines"]]),
            ("capability not a string", ["observe": ["major": 1, "required": [1], "optional": []]]),
            ("capability uppercase", ["observe": ["major": 1, "required": ["State"], "optional": []]]),
            ("capability repeated", ["observe": ["major": 1, "required": ["a", "a"], "optional": []]]),
            ("capability in both", ["observe": ["major": 1, "required": ["a"], "optional": ["a"]]]),
            ("too many capabilities", ["observe": ["major": 1, "required": (0..<33).map { "c\($0)" }, "optional": []]]),
            ("schema without version", ["cards": ["reads": ["town.look"], "writes": []]]),
            ("schema version zero", ["cards": ["reads": ["town.look/v0"], "writes": []]]),
            ("schema uppercase", ["cards": ["reads": ["Town.look/v1"], "writes": []]]),
            ("schema with path", ["cards": ["reads": [], "writes": ["a/b/v1"]]]),
            ("too many schemas", ["cards": ["reads": (0..<17).map { "s\($0)/v1" }, "writes": []]]),
            ("min host zero", ["min_host_version": 0]),
            ("min host boolean", ["min_host_version": true]),
            ("min host string", ["min_host_version": "1"]),
        ]
        for (why, changes) in bad {
            #expect((try? parse(SceneFixture.manifest(changes)).get()) == nil, "\(why)")
        }
        #expect(parse(SceneFixture.manifest(["min_host_version": 2])) == .failure(.needsNewerApp(2)))
    }

    @Test func malformedJSONIsRefused() {
        let base = SceneFixture.manifest()
        #expect(parse(String(base.dropLast())) == .failure(.notJSON))
        #expect(parse("[]") == .failure(.notJSON))
        #expect(parse(base + " x") == .failure(.notJSON))
        // The same key twice, at the top and inside, even spelled with an escape.
        #expect(parse(#"{"name":"A","name":"B"}"#) == .failure(.duplicateKey("name")))
        let nested = base.replacingOccurrences(of: #""major":1"#, with: #""major":1,"major":1"#)
        #expect(parse(nested) == .failure(.duplicateKey("major")))
        // Larger than 16 KiB, even if valid.
        let big = base.replacingOccurrences(of: #""name":"Fixture""#, with: #""name":"Fixture"\#(String(repeating: " ", count: 17_000))"#)
        #expect(parse(big) == .failure(.tooLarge))
    }
}

/// A bundle folder: only plain files of allowed types, within the limits,
/// no links and no way out of the folder.
@Suite struct SceneBundleTests {
    func problem(_ root: URL) -> SceneBundle.Problem? {
        do { _ = try SceneBundle.scan(root); return nil } catch { return error }
    }

    @Test func theFixtureScansWithAStableDigest() throws {
        let a = try SceneBundle.scan(SceneFixture.folder)
        let b = try SceneBundle.scan(try SceneFixture.copy())
        #expect(a.digest == b.digest && a.digest.count == 64)
        #expect(a.files.map(\.path) == ["index.html", "manifest.json", "scene.css", "scene.js"])
    }

    @Test func linksAreRefused() throws {
        let file = try SceneFixture.copy()
        try FileManager.default.createSymbolicLink(atPath: file.appendingPathComponent("extra.js").path, withDestinationPath: "/etc/hosts")
        #expect(problem(file) == .link("extra.js"))

        let dir = try SceneFixture.copy()
        try FileManager.default.createSymbolicLink(atPath: dir.appendingPathComponent("assets").path, withDestinationPath: "/etc")
        #expect(problem(dir) == .link("assets"))

        let hard = try SceneFixture.copy()
        let outside = SceneFixture.scratch("outside").appendingPathComponent("secret.json")
        try Data("{}".utf8).write(to: outside)
        try FileManager.default.linkItem(at: outside, to: hard.appendingPathComponent("secret.json"))
        #expect(problem(hard) == .link("secret.json"))

        let root = SceneFixture.scratch("links").appendingPathComponent("scene")
        try FileManager.default.createSymbolicLink(atPath: root.path, withDestinationPath: SceneFixture.folder.path)
        #expect(problem(root) == .notAFolder)
    }

    @Test func typesNamesAndSpecialFilesAreChecked() throws {
        for (name, expected) in [("tool.exe", SceneBundle.Problem.type("tool.exe")), ("art.svg", .type("art.svg")),
                                 ("code.wasm", .type("code.wasm")), ("README", .type("README")),
                                 (".env.json", .badName(".env.json")), ("-x.js", .badName("-x.js")),
                                 ("a b.js", .badName("a b.js"))] {
            let dir = try SceneFixture.copy()
            try Data().write(to: dir.appendingPathComponent(name))
            #expect(problem(dir) == expected, "\(name)")
        }
        let ok = try SceneFixture.copy()
        try Data("MIT".utf8).write(to: ok.appendingPathComponent("LICENSE"))
        try Data().write(to: ok.appendingPathComponent(".DS_Store"))
        try FileManager.default.createDirectory(at: ok.appendingPathComponent("__MACOSX"), withIntermediateDirectories: true)
        let bundle = try SceneBundle.scan(ok)
        #expect(bundle.files.map(\.path).contains("LICENSE") && !bundle.files.map(\.path).contains(".DS_Store"))

        let fifo = try SceneFixture.copy()
        #expect(mkfifo(fifo.appendingPathComponent("pipe.js").path, 0o600) == 0)
        #expect(problem(fifo) == .special("pipe.js"))

        let none = try SceneFixture.copy()
        try FileManager.default.removeItem(at: none.appendingPathComponent("manifest.json"))
        #expect(problem(none) == .noManifest)

        let noEntry = try SceneFixture.copy()
        try FileManager.default.removeItem(at: noEntry.appendingPathComponent("index.html"))
        #expect(problem(noEntry) == .noEntry)
    }

    @Test func sizesAreBounded() throws {
        func sparse(_ url: URL, _ size: Int) throws {
            FileManager.default.createFile(atPath: url.path, contents: nil)
            let handle = try FileHandle(forWritingTo: url)
            try handle.truncate(atOffset: UInt64(size))
            try handle.close()
        }
        let big = try SceneFixture.copy()
        try sparse(big.appendingPathComponent("big.png"), SceneBundle.maxFile + 1)
        #expect(problem(big) == .fileTooLarge("big.png"))

        let many = try SceneFixture.copy()
        for i in 0..<9 { try sparse(many.appendingPathComponent("part\(i).png"), SceneBundle.maxFile - 1) }
        #expect(problem(many) == .tooLarge)
    }

    @Test func readingNeverLeavesTheBundle() throws {
        let dir = try SceneFixture.copy()
        let outside = dir.deletingLastPathComponent()
        try Data("secret".utf8).write(to: outside.appendingPathComponent("secret.txt"))
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("sub"), withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(atPath: dir.appendingPathComponent("sub/up.txt").path,
                                                   withDestinationPath: "../../secret.txt")
        try FileManager.default.createSymbolicLink(atPath: dir.appendingPathComponent("via").path, withDestinationPath: outside.path)
        for path in ["../secret.txt", "/etc/hosts", "sub/../../secret.txt", "sub//x.txt", "", "./index.html"] {
            #expect(throws: SceneBundle.ReadFailure.path, "\(path)") { try SceneBundle.read(dir, path) }
        }
        #expect(throws: SceneBundle.ReadFailure.link) { try SceneBundle.read(dir, "sub/up.txt") }
        #expect(throws: SceneBundle.ReadFailure.link) { try SceneBundle.read(dir, "via/secret.txt") }
        #expect(try SceneBundle.read(dir, "index.html").count > 0)
    }

    @Test func theSchemeServesOnlyItsOwnScenesFiles() throws {
        let root = SceneFixture.folder
        let id = String(repeating: "a", count: 32), other = String(repeating: "b", count: 32)
        func get(_ raw: String) -> String? {
            URL(string: raw).flatMap { SceneScheme.resolve($0, scene: id, root: root) }?.1
        }
        #expect(get("senate-scene://\(id)/index.html") == "text/html")
        #expect(get("senate-scene://\(id)/scene.js") == "text/javascript")
        for bad in ["senate-scene://\(other)/index.html", "senate-scene://\(id)/../index.html", "senate-scene://\(id)/%2e%2e/index.html",
                    "senate-scene://\(id)//etc/hosts", "senate-scene://\(id)/index.html?x=1", "senate-scene://\(id)/index.html#x",
                    "senate-scene://u@\(id)/index.html", "senate-scene://\(id):1/index.html", "https://\(id)/index.html",
                    "senate-scene://\(id)/missing.js", "senate-scene://\(id)/", "senate-scene://\(id)/index%2Ehtml"] {
            #expect(get(bad) == nil, "\(bad)")
        }
    }

    // MARK: Archives

    func refusal(_ zip: ZipBuilder) -> SceneBundle.Problem? {
        do { try SceneArchive.check(zip.build()); return nil } catch { return error }
    }

    @Test func archivesAreCheckedBeforeUnpacking() {
        var link = ZipBuilder()
        link.add("manifest.json", SceneFixture.manifest())
        link.add("index.html", "/etc/hosts", mode: 0o120777)
        #expect(refusal(link) == .link("index.html"))

        for name in ["../evil.js", "/abs.js", "a\\b.js", "a/../../b.js", "__MACOSX/../x.js"] {
            var zip = ZipBuilder()
            zip.add(name, "x")
            #expect(refusal(zip) == .badName(name), "\(name)")
        }
        var type = ZipBuilder()
        type.add("run.sh", "x")
        #expect(refusal(type) == .type("run.sh"))

        var bomb = ZipBuilder()
        bomb.add("a.png", "x", claimedSize: UInt32(SceneBundle.maxFile + 1))
        #expect(refusal(bomb) == .fileTooLarge("a.png"))

        var total = ZipBuilder()
        for i in 0..<9 { total.add("p\(i).png", "x", claimedSize: UInt32(SceneBundle.maxFile - 1)) }
        #expect(refusal(total) == .tooLarge)

        var encrypted = ZipBuilder()
        encrypted.add("a.js", "x", flags: 1)
        #expect(refusal(encrypted) != nil)

        var deflated = ZipBuilder()
        deflated.add("a.js", "x", method: 8)
        #expect(refusal(deflated) == .archive(L("scenes.problem.archive_compressed")))

        #expect(refusal(ZipBuilder()) == nil)
        #expect((try? SceneArchive.check(Data("not a zip".utf8))) == nil)
    }

    @MainActor
    @Test func aGoodArchiveUnpacksAndScans() throws {
        var zip = ZipBuilder()
        // Zipped inside one folder, as Finder does, with its sidecars.
        for name in ["manifest.json", "index.html", "scene.js", "scene.css"] {
            zip.add("Fixture/\(name)", String(decoding: try Data(contentsOf: SceneFixture.folder.appendingPathComponent(name)), as: UTF8.self))
        }
        zip.add("__MACOSX/Fixture/._index.html", "x")
        let file = SceneFixture.scratch("zips").appendingPathComponent("fixture.senatescene")
        try zip.build().write(to: file)
        let library = SceneFixture.library()
        let candidate = try library.inspect(file)
        #expect(candidate.bundle.digest == (try SceneBundle.scan(SceneFixture.folder)).digest)
        let staging = try #require(candidate.staging)
        library.discard(candidate)
        #expect(!FileManager.default.fileExists(atPath: staging.path))
    }
}
