Pod::Spec.new do |s|
  s.name           = 'OctetLive'
  s.version        = '0.1.0'
  s.summary        = 'The Octet Live Activity: ActivityKit for the Dynamic Island and the Lock Screen.'
  s.description    = s.summary
  s.author         = 'Octet'
  s.homepage       = 'https://docs.expo.dev/modules/'
  s.license        = { :type => 'MIT' }
  # The app's floor (Expo's 16.4). ActivityKit is used only behind `#available(iOS 17.2, *)`, the
  # first with push-to-start tokens; below it the module reports unsupported and does nothing.
  s.platforms      = {
    :ios => '16.4'
  }
  s.source         = { git: '' }
  s.static_framework = true
  s.swift_version  = '5.9'

  s.dependency 'ExpoModulesCore'
  s.frameworks = 'ActivityKit'

  s.source_files = '**/*.{h,m,swift}'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'SWIFT_COMPILATION_MODE' => 'wholemodule'
  }
end
