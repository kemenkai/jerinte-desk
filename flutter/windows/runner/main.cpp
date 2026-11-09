#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <tchar.h>
#include <uni_links_desktop/uni_links_desktop_plugin.h>
#include <windows.h>

#include <algorithm>
#include <iostream>

#include "win32_desktop.h"
#include "flutter_window.h"
#include "utils.h"
#include "service_control.h"

typedef char** (*FUNC_RUSTDESK_CORE_MAIN)(int*);
typedef void (*FUNC_RUSTDESK_FREE_ARGS)( char**, int);
typedef int (*FUNC_RUSTDESK_GET_APP_NAME)(wchar_t*, int);
typedef int (*FUNC_RUSTDESK_IS_DISABLE_INSTALLATION)();
/// Note: `--server`, `--service` are already handled in [core_main.rs].
const std::vector<std::string> parameters_white_list = {"--install", "--cm"};

const wchar_t* getWindowClassName();

// Without librustdesk.dll the app can do nothing, and a double-click used to end in
// silence: an antivirus that quarantined the DLL, or an install that left it empty,
// looked like an app that does not react. Only a plain launch gets this dialog --
// the service, tray and connection-manager processes run with arguments, some in
// session 0, where a message box would wait forever with nobody to click it.
static void ShowCoreLoadFailure(DWORD error)
{
  wchar_t path[MAX_PATH] = {0};
  GetModuleFileNameW(nullptr, path, MAX_PATH);
  std::wstring app(path);
  app = app.substr(app.find_last_of(L"\\/") + 1);
  app = app.substr(0, app.find_last_of(L'.'));
  if (app.empty()) {
    app = L"RustDesk";
  }
  const std::wstring code = std::to_wstring(error);
  // Chinese first, then English. The Chinese text is written as \u escapes: this
  // file is compiled without /utf-8 and with /WX, so any non-ASCII byte in it would
  // fail the build with C4819. It reads: "{app} cannot start: the program file
  // librustdesk.dll is missing or cannot be loaded (error code {code}). Antivirus
  // software may have blocked or quarantined it. Trust {app}'s installation folder
  // in the antivirus, then right-click the installer, choose 'Run as administrator'
  // and install again."
  std::wstring message =
      app + L" \u65e0\u6cd5\u542f\u52a8\uff1a\u7a0b\u5e8f\u6587\u4ef6 librustdesk.dll "
            L"\u7f3a\u5931\u6216\u65e0\u6cd5\u52a0\u8f7d\uff08\u9519\u8bef\u7801 " +
      code + L"\uff09\u3002\n"
             L"\u5b83\u53ef\u80fd\u88ab\u6740\u6bd2\u8f6f\u4ef6\u62e6\u622a\u6216\u9694\u79bb"
             L"\u4e86\u3002\u8bf7\u5728\u6740\u6bd2\u8f6f\u4ef6\u91cc\u628a " +
      app + L" \u7684\u5b89\u88c5\u76ee\u5f55\u52a0\u5165\u4fe1\u4efb\uff0c\u7136\u540e"
            L"\u53f3\u952e\u5b89\u88c5\u5305\uff0c\u9009\u62e9\u300c\u4ee5\u7ba1\u7406\u5458"
            L"\u8eab\u4efd\u8fd0\u884c\u300d\u91cd\u65b0\u5b89\u88c5\u3002\n\n" +
      app + L" cannot start: its program file librustdesk.dll is missing or cannot be loaded "
            L"(error " + code + L").\n"
            L"Antivirus software may have blocked or quarantined it. Allow " + app +
      L"'s installation folder in your antivirus, then right-click the installer, choose "
      L"\"Run as administrator\" and install again.";
  MessageBoxW(nullptr, message.c_str(), app.c_str(),
              MB_OK | MB_ICONERROR | MB_SETFOREGROUND);
}

int APIENTRY wWinMain(_In_ HINSTANCE instance, _In_opt_ HINSTANCE prev,
                      _In_ wchar_t *command_line, _In_ int show_command)
{
  if (auto result = service_control::HandleCommand(command_line)) {
    return static_cast<int>(*result);
  }
  HINSTANCE hInstance = LoadLibraryA("librustdesk.dll");
  if (!hInstance)
  {
    const DWORD error = GetLastError();
    std::cout << "Failed to load librustdesk.dll, error " << error << "." << std::endl;
    std::wstring args = command_line ? command_line : L"";
    if (args.find_first_not_of(L" \t") == std::wstring::npos) {
      ShowCoreLoadFailure(error);
    }
    return EXIT_FAILURE;
  }
  FUNC_RUSTDESK_CORE_MAIN rustdesk_core_main =
      (FUNC_RUSTDESK_CORE_MAIN)GetProcAddress(hInstance, "rustdesk_core_main_args");
  if (!rustdesk_core_main)
  {
    std::cout << "Failed to get rustdesk_core_main." << std::endl;
    return EXIT_FAILURE;
  }
  FUNC_RUSTDESK_FREE_ARGS free_c_args =
      (FUNC_RUSTDESK_FREE_ARGS)GetProcAddress(hInstance, "free_c_args");
  if (!free_c_args)
  {
    std::cout << "Failed to get free_c_args." << std::endl;
    return EXIT_FAILURE;
  }
  std::vector<std::string> command_line_arguments =
      GetCommandLineArguments();
  // Remove possible trailing whitespace from command line arguments
  for (auto& argument : command_line_arguments) {
    argument.erase(argument.find_last_not_of(" \n\r\t"));
  }

  int args_len = 0;
  char** c_args = rustdesk_core_main(&args_len);
  if (!c_args)
  {
    std::string args_str = "";
    for (const auto& argument : command_line_arguments) {
      args_str += (argument + " ");
    }
    // std::cout << "RustDesk [" << args_str << "], core returns false, exiting without launching Flutter app." << std::endl;
    return EXIT_SUCCESS;
  }
  std::vector<std::string> rust_args(c_args, c_args + args_len);
  free_c_args(c_args, args_len);
  FUNC_RUSTDESK_IS_DISABLE_INSTALLATION rustdesk_is_disable_installation =
      (FUNC_RUSTDESK_IS_DISABLE_INSTALLATION)GetProcAddress(hInstance, "rustdesk_is_disable_installation");
  bool is_disable_installation =
      rustdesk_is_disable_installation && rustdesk_is_disable_installation() != 0;
  const auto installParam = std::string("--install");
  // Flutter reads the original process command line, not only rust_args, so
  // remove the `--install` injected by the portable wrapper here as well. This
  // also lets `no-install.exe` continue as a portable app when installation is
  // disabled. See: https://github.com/rustdesk/rustdesk-server-pro/issues/991#issuecomment-4978376890
  if (is_disable_installation) {
    command_line_arguments.erase(
        std::remove(command_line_arguments.begin(),
                    command_line_arguments.end(),
                    installParam),
        command_line_arguments.end());
  }

  std::wstring app_name = L"RustDesk";
  FUNC_RUSTDESK_GET_APP_NAME get_rustdesk_app_name = (FUNC_RUSTDESK_GET_APP_NAME)GetProcAddress(hInstance, "get_rustdesk_app_name");
  if (get_rustdesk_app_name) {
    wchar_t app_name_buffer[512] = {0};
    if (get_rustdesk_app_name(app_name_buffer, 512) == 0) {
      app_name = std::wstring(app_name_buffer);
    }
  }

  // Uri links dispatch
  HWND hwnd = ::FindWindowW(getWindowClassName(), app_name.c_str());
  if (hwnd != NULL) {
    // Allow multiple flutter instances when being executed by parameters
    // contained in whitelists.
    bool allow_multiple_instances = false;
    for (auto& whitelist_param : parameters_white_list) {
      allow_multiple_instances =
          allow_multiple_instances ||
          std::find(command_line_arguments.begin(),
                    command_line_arguments.end(),
                    whitelist_param) != command_line_arguments.end();
    }
    if (!allow_multiple_instances) {
      if (!command_line_arguments.empty()) {
        // The process launched by the browser owns the foreground permission.
        // Transfer it to the existing RustDesk process before dispatching the
        // URI so that it can bring an existing session to the foreground.
        DWORD pid = 0;
        ::GetWindowThreadProcessId(hwnd, &pid);
        if (pid != 0) {
          ::AllowSetForegroundWindow(pid);
        }
        // Dispatch command line arguments
        DispatchToUniLinksDesktop(hwnd);
      } else {
        // Not called with arguments, or just open the app shortcut on desktop.
        // So we just show the main window instead.
        ::ShowWindow(hwnd, SW_NORMAL);
        ::SetForegroundWindow(hwnd);
      }
      return EXIT_FAILURE;
    }
  }

  // Attach to console when present (e.g., 'flutter run') or create a
  // new console when running with a debugger.
  if (!::AttachConsole(ATTACH_PARENT_PROCESS) && ::IsDebuggerPresent())
  {
    CreateAndAttachConsole();
  }

  // Initialize COM, so that it is available for use in the library and/or
  // plugins.
  ::CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

  flutter::DartProject project(L"data");
  // connection manager hide icon from taskbar
  bool is_cm_page = false;
  auto cmParam = std::string("--cm");
  if (!command_line_arguments.empty() && command_line_arguments.front().compare(0, cmParam.size(), cmParam.c_str()) == 0) {
    is_cm_page = true;
  }
  bool is_install_page = false;
  if (!command_line_arguments.empty() && command_line_arguments.front().compare(0, installParam.size(), installParam.c_str()) == 0) {
    is_install_page = true;
  }

  command_line_arguments.insert(command_line_arguments.end(), rust_args.begin(), rust_args.end());
  project.set_dart_entrypoint_arguments(std::move(command_line_arguments));

  FlutterWindow window(project);

  // Get primary monitor's work area.
  Win32Window::Point workarea_origin(0, 0);
  Win32Window::Size workarea_size(0, 0);

  Win32Desktop::GetWorkArea(workarea_origin, workarea_size);

  // Compute window bounds for default main window position: (10, 10) x(800, 600)
  Win32Window::Point relative_origin(10, 10);

  Win32Window::Point origin(workarea_origin.x + relative_origin.x, workarea_origin.y + relative_origin.y);
  Win32Window::Size size(800u, 600u);

  // Fit the window to the monitor's work area.
  Win32Desktop::FitToWorkArea(origin, size);

  std::wstring window_title;
  if (is_cm_page) {
    window_title = app_name + L" - Connection Manager";
  } else if (is_install_page) {
    window_title = app_name + L" - Install";
  } else {
    window_title = app_name;
  }
  if (!window.CreateAndShow(window_title, origin, size, !is_cm_page)) {
      return EXIT_FAILURE;
  }
  window.SetQuitOnClose(true);

  ::MSG msg;
  while (::GetMessage(&msg, nullptr, 0, 0))
  {
    ::TranslateMessage(&msg);
    ::DispatchMessage(&msg);
  }

  ::CoUninitialize();
  return EXIT_SUCCESS;
}
