import React from 'react';
import { SafeAreaView, Text } from 'react-native';
import AccountScreen from './src/screens/AccountScreen';

export default function App() {
  return (
    <SafeAreaView style={{flex:1}}>
      <AccountScreen />
    </SafeAreaView>
  );
}
