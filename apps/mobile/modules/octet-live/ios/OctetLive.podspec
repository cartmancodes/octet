Pod::Spec.new do |s|
  s.name           = 'OctetLive'
  s.version        = '0.1.0'
  s.summary        = 'The Octet Live Activity: ActivityKit for the Dynamic Island and the Lock Screen.'
  s.description    = s.summary
  s.author         = 'Octet'
  s.homepage       = 'https://docs.expo.dev/modules/'
  s.license        = { :type => 'MIT' }
  # pushToStartTokenUpdates arrived in iOS 17.2; app.json sets the app's deployment target to match.
  s.platforms      = {
    :ios => '17.2'
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
